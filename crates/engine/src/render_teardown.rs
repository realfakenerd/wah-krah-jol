//! Brings the render app back to the main thread once the app has decided to exit, so shutdown
//! cannot deadlock against the render thread.
//!
//! With pipelined rendering the render app spends each frame on its own thread, and the main
//! thread takes it back in the next extract. Three Bevy behaviours combine into a deadlock at
//! exit:
//!
//! - `RenderAppChannels`'s `Drop` (`bevy_render/src/pipelined_rendering.rs`) blocks on the render
//!   thread with `recv_blocking`, without ticking the `MainThreadExecutor`. Any render system that
//!   must run on the main thread (on macOS and iOS, `create_surfaces`) is queued on that executor,
//!   so the last render frame can never finish and the drop never returns.
//! - bevy_winit's `about_to_wait` (`bevy_winit/src/state.rs`) can run another `App::update` after
//!   `event_loop.exit()`; with only hidden windows every `AboutToWait` updates.
//! - That extra update re-extracts a `Window` whose surface `WindowClosing` already dropped, so
//!   the render frame needs a new surface and runs `create_surfaces`. The drop then happens in
//!   bevy_winit's `exiting` handler (`World::clear_all`), while that frame is out on the render
//!   thread.
//!
//! [`install`] wraps the `RenderExtractApp` extract: after the first extract that leaves an
//! [`AppExit`] pending it waits for that frame the way Bevy's own per-frame extract does, ticking
//! the main-thread executor, then keeps the render app on the main thread and skips every later
//! extract. The channels' `Drop` then has nothing to wait for, and later updates never reach the
//! render thread. Bevy 0.20 makes `Drop for RenderAppChannels` tick the main-thread executor
//! (bevyengine/bevy#25722), which removes the deadlock; this module can go with that upgrade.

use bevy::{
    app::{AppExit, SubApp},
    ecs::schedule::MainThreadExecutor,
    platform::time::Instant,
    prelude::*,
    render::pipelined_rendering::{RenderAppChannels, RenderExtractApp},
    tasks::ComputeTaskPool,
};

/// Wraps the pipelined-rendering extract so the render app is back on the main thread before
/// shutdown starts.
///
/// Call after the render plugins are added (`PipelinedRenderingPlugin::build` sets the extract
/// being wrapped) and before [`App::run`] (its `cleanup` starts the render thread). Returns false,
/// changing nothing, when the app has no pipelined render thread.
pub(crate) fn install(app: &mut App) -> bool {
    let Some(sub_app) = app.get_sub_app_mut(RenderExtractApp) else {
        return false;
    };
    let Some(mut extract) = sub_app.take_extract() else {
        return false;
    };
    // `Some` once the render app has been taken back. The render app is held here rather than
    // dropped in the extract: dropped there, `RenderDiagnosticsPlugin` logs a failed statistics
    // buffer download on most exits. Held here, it is dropped with the `App`, on the main thread,
    // after bevy_winit has dropped the winit windows - the same order as an exit without this
    // guard.
    let mut retired: Option<Option<SubApp>> = None;
    sub_app.set_extract(move |main_world, extract_world| {
        if retired.is_some() {
            return;
        }
        // Checked after the extract, not before: Bevy's render error handler writes `AppExit`
        // from inside the render app's extract, and nothing guarantees another update follows.
        extract(main_world, extract_world);
        if exit_requested(main_world) {
            let started = Instant::now();
            let render_app = retire_render_app(main_world);
            debug!(
                elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                returned = render_app.is_some(),
                "render app taken back before shutdown"
            );
            retired = Some(render_app);
        }
    });
    true
}

/// `App::should_exit` reports an exit for any `AppExit` message still readable, so this matches
/// the update after which the runner stops.
fn exit_requested(world: &World) -> bool {
    world
        .get_resource::<Messages<AppExit>>()
        .is_some_and(|messages| !messages.is_empty())
}

/// Waits for the render frame in flight and returns its render app, running main-thread render
/// systems while it waits, as `renderer_extract` does every frame. `None` when pipelined rendering
/// never started or the render thread has gone.
fn retire_render_app(main_world: &mut World) -> Option<SubApp> {
    if !main_world.contains_resource::<RenderAppChannels>()
        || !main_world.contains_resource::<MainThreadExecutor>()
    {
        return None;
    }
    main_world.resource_scope(|world, executor: Mut<MainThreadExecutor>| {
        world.resource_scope(|_, mut channels: Mut<RenderAppChannels>| {
            ComputeTaskPool::get()
                .scope_with_executor(true, Some(&*executor.0), |scope| {
                    scope.spawn(async { channels.recv().await });
                })
                .pop()
                .flatten()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::install;
    use bevy::{
        app::{AppExit, SubApp, TaskPoolPlugin},
        ecs::{schedule::ScheduleLabel, system::NonSendMarker},
        prelude::*,
        render::{RenderApp, pipelined_rendering::PipelinedRenderingPlugin},
    };
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };

    /// Generous: each step takes milliseconds when it passes, the margin is for a loaded machine.
    const STEP_TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(ScheduleLabel, Debug, Clone, PartialEq, Eq, Hash)]
    struct FakeRender;

    /// Runs Bevy's real pipelined rendering with a GPU-free render app whose only system must run
    /// on the main thread, like `create_surfaces` on macOS. Does two ordinary updates, one that
    /// raises `AppExit`, `extra_updates` more (bevy_winit can run one after exiting), then what
    /// bevy_winit's `exiting` does (`World::clear_all`) and the drop.
    ///
    /// The `App` lives on its own thread, which is that app's main thread, so a deadlock leaves
    /// that thread blocked and the test still ends. `teardown_timeout` only starts once that thread
    /// has finished its updates, so a slow start on a loaded machine cannot read as a deadlock.
    /// Returns whether teardown finished within it, and how many render frames ran.
    fn shut_down(
        with_guard: bool,
        extra_updates: usize,
        teardown_timeout: Duration,
    ) -> (bool, usize) {
        let frames = Arc::new(AtomicUsize::new(0));
        let (progress, steps) = mpsc::channel();
        let counter = frames.clone();
        std::thread::spawn(move || {
            let mut app = App::new();
            app.add_plugins(TaskPoolPlugin::default());
            let mut render_app = SubApp::new();
            render_app.update_schedule = Some(FakeRender.intern());
            render_app.add_systems(FakeRender, move |_: NonSendMarker| {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            app.insert_sub_app(RenderApp, render_app);
            app.add_plugins(PipelinedRenderingPlugin);
            if with_guard {
                assert!(install(&mut app), "pipelined rendering should be present");
            }
            app.finish();
            app.cleanup();
            app.update();
            app.update();
            app.world_mut().write_message(AppExit::Success);
            app.update();
            for _ in 0..extra_updates {
                app.update();
            }
            progress.send(()).ok();
            app.world_mut().clear_all();
            drop(app);
            progress.send(()).ok();
        });
        steps
            .recv_timeout(STEP_TIMEOUT)
            .expect("the app thread should finish its updates");
        let completed = match steps.recv_timeout(teardown_timeout) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the app thread panicked in teardown")
            }
        };
        (completed, frames.load(Ordering::SeqCst))
    }

    #[test]
    fn shutdown_completes_when_the_last_render_frame_needs_the_main_thread() {
        assert_eq!(shut_down(true, 0, STEP_TIMEOUT), (true, 3));
    }

    #[test]
    fn updates_after_the_exit_do_not_reach_the_render_thread() {
        assert_eq!(shut_down(true, 2, STEP_TIMEOUT), (true, 3));
    }

    #[test]
    fn bevy_alone_still_blocks_this_shutdown() {
        assert_eq!(
            shut_down(false, 0, Duration::from_secs(2)),
            (false, 2),
            "Bevy no longer deadlocks dropping the render app channels while a main-thread render \
             system is pending (bevyengine/bevy#25722); render_teardown can be removed"
        );
    }

    #[test]
    fn install_reports_no_render_thread() {
        let mut app = App::new();
        app.add_plugins(TaskPoolPlugin::default());
        assert!(!install(&mut app));
        app.update();
    }
}
