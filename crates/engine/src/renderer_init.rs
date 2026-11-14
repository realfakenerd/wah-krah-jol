//! Adapter-aware initialization for the pinned Bevy renderer.

use bevy::{
    app::Plugin,
    prelude::{App, Res, Resource, Startup},
    render::{
        RenderPlugin,
        renderer::{RenderAdapterInfo, RenderDevice},
        settings::WgpuSettings,
    },
};

/// Installs Bevy's renderer after applying the selected-backend feature policy.
///
/// Insert this at the original `RenderPlugin` position in `DefaultPlugins`, with
/// that entry disabled, so subsequent render plugins retain their usual order.
#[derive(Default)]
pub struct RendererInitPlugin(WgpuSettings);

impl Plugin for RendererInitPlugin {
    /// Adds `RenderPlugin`, initialized manually when DX12 may be selected.
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, log_renderer_policy);

        #[cfg(not(target_os = "windows"))]
        {
            let settings = settings_without_dx12(self.0.clone());
            app.insert_resource(RequestedInstance::of(&settings));
            app.add_plugins(RenderPlugin {
                render_creation: settings.into(),
                ..Default::default()
            });
        }

        #[cfg(target_os = "windows")]
        {
            app.insert_resource(RequestedInstance::of(&self.0));
            use bevy::{
                ecs::query::With,
                render::settings::RenderCreation,
                window::{PrimaryWindow, RawHandleWrapperHolder},
            };

            let settings = &self.0;
            let backends = settings
                .backends
                .filter(|mask| mask.contains(wgpu::Backends::DX12));
            let Some(backends) = backends else {
                // Keep Bevy's own initialization when DX12 cannot be selected,
                // including the explicit renderer-disabled setting.
                app.add_plugins(RenderPlugin {
                    render_creation: settings.clone().into(),
                    ..Default::default()
                });
                return;
            };
            let primary_window = app
                .world_mut()
                .query_filtered::<&RawHandleWrapperHolder, With<PrimaryWindow>>()
                .single(app.world())
                .ok()
                .cloned();
            let resources =
                bevy::tasks::block_on(initialize_renderer(backends, primary_window, settings));
            app.add_plugins(RenderPlugin {
                render_creation: RenderCreation::Manual(resources),
                ..Default::default()
            });
        }
    }
}

/// The instance settings handed to wgpu, kept so the startup log can state them.
///
/// wgpu exposes no getter for the flags an instance was created with, so this
/// records the request rather than reading the state back.
#[derive(Resource, Clone, Copy)]
struct RequestedInstance {
    backends: Option<wgpu::Backends>,
    flags: wgpu::InstanceFlags,
}

impl RequestedInstance {
    fn of(settings: &WgpuSettings) -> Self {
        Self {
            backends: settings.backends,
            flags: settings.instance_flags,
        }
    }
}

/// Removes the DX12 bit from the backend list where DX12 cannot exist.
///
/// Bevy keeps wgpu's indirect-call validation in release builds whenever the
/// backend list contains `Backends::DX12`, and its default list is
/// `Backends::all()` on every platform. Off Windows that bit can never select
/// a backend, yet it leaves the validation on: wgpu then batches every
/// multi-draw entry on the CPU. Bevy decides this once, inside
/// `WgpuSettings::default()`, so changing `backends` afterwards does not move
/// the stored flags; this redoes the same decision for the reduced list.
/// Environment overrides keep the final say: `WGPU_BACKEND` is already part of
/// the incoming list, and `with_env` re-applies `WGPU_VALIDATION_INDIRECT_CALL`
/// (and the other `WGPU_*` flag variables) over the result, as Bevy does.
#[cfg(any(not(target_os = "windows"), test))]
fn settings_without_dx12(mut settings: WgpuSettings) -> WgpuSettings {
    // `None` is Bevy's explicit renderer-disabled setting and stays that way.
    settings.backends = settings
        .backends
        .map(|backends| backends.difference(wgpu::Backends::DX12));
    // Debug builds keep the validation, matching Bevy's own rule.
    #[cfg(not(debug_assertions))]
    {
        settings
            .instance_flags
            .remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
        settings.instance_flags = settings.instance_flags.with_env();
    }
    settings
}

/// Reports the actual renderer policy once, after Bevy installs its resources.
fn log_renderer_policy(
    adapter: Option<Res<RenderAdapterInfo>>,
    device: Option<Res<RenderDevice>>,
    requested: Option<Res<RequestedInstance>>,
) {
    if let (Some(adapter), Some(device)) = (adapter, device) {
        bevy::log::info!(
            "Renderer backend={:?} dx12_indirect_count_workaround={} indirect_count_enabled={}",
            adapter.backend,
            adapter.backend == wgpu::Backend::Dx12,
            device
                .features()
                .contains(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT),
        );
        if let Some(requested) = requested {
            // The flag is requested at instance creation; wgpu has no getter
            // to read it back from the live instance.
            bevy::log::info!(
                "Renderer instance requested_backends={:?} requested_validation_indirect_call={}",
                requested.backends,
                requested
                    .flags
                    .contains(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL),
            );
        }
    }
}

/// Removes only the feature whose producer/consumer contracts disagree on DX12.
#[cfg(any(target_os = "windows", test))]
fn backend_features(backend: wgpu::Backend, mut features: wgpu::Features) -> wgpu::Features {
    // Bevy 0.19 compacts indirect commands when COUNT is enabled, but its draw
    // consumer uses fixed ranges on DX12 because of wgpu #7974. Those ranges can
    // include stale commands. Use the actual selected backend, including Auto.
    // https://github.com/gfx-rs/wgpu/issues/7974
    // Remove this workaround once Bevy shares a backend-qualified capability
    // check between indirect command generation and drawing.
    if backend == wgpu::Backend::Dx12 {
        features.remove(wgpu::Features::MULTI_DRAW_INDIRECT_COUNT);
    }
    features
}

/// Retains Bevy's explicit-feature precedence before the backend safety policy.
#[cfg(any(target_os = "windows", test))]
fn configured_features(
    backend: wgpu::Backend,
    mut automatic: wgpu::Features,
    disabled: Option<wgpu::Features>,
    explicit: wgpu::Features,
) -> wgpu::Features {
    if let Some(disabled) = disabled {
        automatic.remove(disabled);
    }
    // Apply after explicit features as well, so an explicit COUNT request cannot
    // reintroduce the mismatch. All other Bevy feature precedence is unchanged.
    backend_features(backend, automatic | explicit)
}

/// Selects one adapter and applies the DX12 policy before its only device request.
#[cfg(target_os = "windows")]
async fn initialize_renderer(
    backends: wgpu::Backends,
    primary_window: Option<bevy::window::RawHandleWrapperHolder>,
    options: &bevy::render::settings::WgpuSettings,
) -> bevy::render::settings::RenderResources {
    use std::sync::Arc;

    use bevy::render::{
        renderer::{
            RenderAdapter, RenderAdapterInfo, RenderDevice, RenderInstance, RenderQueue,
            WgpuWrapper,
        },
        settings::{RenderResources, WgpuSettingsPriority},
    };

    // Keep the instance, selection, limits and device policy of pinned Bevy
    // 0.19's renderer::initialize_renderer. Its public API has no hook between
    // adapter selection and the device request. Recheck this copy on Bevy
    // upgrades or if enabling raw_vulkan_init. Request exactly one device.
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends,
        flags: options.instance_flags,
        memory_budget_thresholds: options.instance_memory_budget_thresholds,
        display: None,
        backend_options: wgpu::BackendOptions {
            gl: wgpu::GlBackendOptions {
                gles_minor_version: options.gles3_minor_version,
                fence_behavior: wgpu::GlFenceBehavior::Normal,
                debug_fns: wgpu::GlDebugFns::Auto,
            },
            dx12: wgpu::Dx12BackendOptions {
                shader_compiler: options.dx12_shader_compiler.clone(),
                presentation_system: wgpu::wgt::Dx12SwapchainKind::from_env().unwrap_or_default(),
                latency_waitable_object: wgpu::wgt::Dx12UseFrameLatencyWaitableObject::from_env()
                    .unwrap_or_default(),
                force_shader_model: wgpu::ForceShaderModelToken::default(),
                agility_sdk: None,
            },
            noop: wgpu::NoopBackendOptions { enable: false },
        },
    });
    let surface = primary_window.and_then(|holder| {
        let wrapper = holder
            .0
            .lock()
            .expect("Couldn't get the window handle in time for renderer initialization");
        wrapper.as_ref().map(|wrapper| {
            // SAFETY: Plugin build runs on the main thread, just as Bevy's
            // automatic renderer initialization does. The wrapper owns the
            // window handle for the lifetime of the resulting surface.
            let handle = unsafe { wrapper.get_handle() };
            instance
                .create_surface(handle)
                .expect("Failed to create wgpu surface")
        })
    });
    let force_fallback_adapter = std::env::var("WGPU_FORCE_FALLBACK_ADAPTER")
        .map_or(options.force_fallback_adapter, |value| {
            !(value.is_empty() || value == "0" || value == "false")
        });
    let desired_adapter_name = std::env::var("WGPU_ADAPTER_NAME").map_or_else(
        |_| options.adapter_name.clone(),
        |name| Some(name.to_lowercase()),
    );
    let request_options = wgpu::RequestAdapterOptions {
        power_preference: options.power_preference,
        compatible_surface: surface.as_ref(),
        force_fallback_adapter,
    };
    let mut selected_adapter = None;
    if let Some(adapter_name) = desired_adapter_name {
        for adapter in instance.enumerate_adapters(backends).await {
            bevy::log::trace!("Checking adapter: {:?}", adapter.get_info());
            if surface
                .as_ref()
                .is_some_and(|surface| !adapter.is_surface_supported(surface))
            {
                continue;
            }
            if adapter
                .get_info()
                .name
                .to_lowercase()
                .contains(&adapter_name.to_lowercase())
            {
                selected_adapter = Some(adapter);
                break;
            }
        }
    }
    if selected_adapter.is_none() {
        bevy::log::debug!("Searching for adapter with options: {:?}", request_options);
        selected_adapter = instance.request_adapter(&request_options).await.ok();
    }
    let adapter = selected_adapter.expect(
        "Unable to find a GPU! Make sure you have installed required drivers! \
         See https://bevy.org/learn/errors/b0006/",
    );
    let adapter_info = adapter.get_info();
    bevy::log::info!("{:?}", adapter_info);
    if adapter_info.device_type == wgpu::DeviceType::Cpu {
        bevy::log::warn!(
            "The selected adapter is using a driver that only supports software rendering. \
             This is likely to be very slow. See https://bevy.org/learn/errors/b0006/"
        );
    }

    let mut features = wgpu::Features::empty();
    let mut limits = options.limits.clone();
    if matches!(options.priority, WgpuSettingsPriority::Functionality) {
        features = adapter.features();
        if adapter_info.device_type == wgpu::DeviceType::DiscreteGpu {
            features.remove(wgpu::Features::MAPPABLE_PRIMARY_BUFFERS);
        }
        limits = adapter.limits();
    }
    features = configured_features(
        adapter_info.backend,
        features,
        options.disabled_features,
        options.features,
    );
    if let Some(constrained_limits) = options.constrained_limits.as_ref() {
        limits = limits.or_worse_values_from(constrained_limits);
    }
    let descriptor = wgpu::DeviceDescriptor {
        label: options.device_label.as_deref(),
        required_features: features,
        required_limits: limits,
        // SAFETY: Matches pinned Bevy's initialization policy; this is required
        // for the features it selects. See bevyengine/bevy#22082.
        experimental_features: unsafe { wgpu::ExperimentalFeatures::enabled() },
        memory_hints: options.memory_hints.clone(),
        trace: wgpu::Trace::Off,
    };
    let (device, queue) = adapter.request_device(&descriptor).await.unwrap();
    bevy::log::debug!("Configured wgpu adapter Limits: {:#?}", device.limits());
    bevy::log::debug!("Configured wgpu adapter Features: {:#?}", device.features());
    RenderResources(
        RenderDevice::from(device),
        RenderQueue(Arc::new(WgpuWrapper::new(queue))),
        RenderAdapterInfo(WgpuWrapper::new(adapter_info)),
        RenderAdapter(Arc::new(WgpuWrapper::new(adapter))),
        RenderInstance(Arc::new(WgpuWrapper::new(instance))),
    )
}

#[cfg(test)]
mod tests {
    use super::{backend_features, configured_features, settings_without_dx12};
    use bevy::render::settings::WgpuSettings;
    use wgpu::{Backend, Backends, Features};

    /// The default list names DX12, and the off-Windows settings must not.
    #[test]
    fn default_settings_lose_the_dx12_bit() {
        let _env = env_guard();
        let defaults = WgpuSettings::default();
        let settings = settings_without_dx12(defaults.clone());
        let backends = settings.backends.expect("a backend list is always set");
        assert!(!backends.contains(Backends::DX12));
        // Only the DX12 bit goes; the rest of the default list is kept.
        if defaults.backends == Some(Backends::all()) {
            assert_eq!(backends, Backends::all() & !Backends::DX12);
        }
    }

    /// A disabled renderer stays disabled, and a user list keeps all its other bits.
    #[test]
    fn user_lists_lose_only_the_dx12_bit() {
        let _env = env_guard();
        let disabled = settings_without_dx12(WgpuSettings {
            backends: None,
            ..Default::default()
        });
        assert_eq!(disabled.backends, None);

        let mixed = settings_without_dx12(WgpuSettings {
            backends: Some(Backends::DX12 | Backends::METAL),
            ..Default::default()
        });
        assert_eq!(mixed.backends, Some(Backends::METAL));

        let dx12_only = settings_without_dx12(WgpuSettings {
            backends: Some(Backends::DX12),
            ..Default::default()
        });
        assert_eq!(dx12_only.backends, Some(Backends::empty()));

        let untouched = settings_without_dx12(WgpuSettings {
            backends: Some(Backends::VULKAN | Backends::METAL),
            ..Default::default()
        });
        assert_eq!(untouched.backends, Some(Backends::VULKAN | Backends::METAL));

        let empty = settings_without_dx12(WgpuSettings {
            backends: Some(Backends::empty()),
            ..Default::default()
        });
        assert_eq!(empty.backends, Some(Backends::empty()));
    }

    /// Serializes every test that reads or changes the process environment.
    /// Release builds read `WGPU_VALIDATION_INDIRECT_CALL` inside
    /// `settings_without_dx12`, so all callers of it take this lock, not only
    /// the override tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs `body` with `WGPU_VALIDATION_INDIRECT_CALL` set to `value` (or unset), then restores it.
    #[cfg(not(debug_assertions))]
    fn with_indirect_validation_env(value: Option<&str>, body: impl FnOnce()) {
        const KEY: &str = "WGPU_VALIDATION_INDIRECT_CALL";
        let _guard = env_guard();
        let previous = std::env::var_os(KEY);
        // SAFETY: every test that reads or writes this variable, directly or
        // through `WgpuSettings::default()` and `settings_without_dx12`, holds
        // `ENV_LOCK`, so no other thread in this module touches the environment
        // while it is changed.
        unsafe {
            match value {
                Some(value) => std::env::set_var(KEY, value),
                None => std::env::remove_var(KEY),
            }
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        // SAFETY: as above.
        unsafe {
            match previous {
                Some(previous) => std::env::set_var(KEY, previous),
                None => std::env::remove_var(KEY),
            }
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// Release builds drop the indirect-call validation unless the environment asks for it.
    /// Debug builds keep it, so this only runs under `cargo test --release`.
    #[cfg(not(debug_assertions))]
    #[test]
    fn release_settings_drop_indirect_call_validation() {
        with_indirect_validation_env(None, || {
            let settings = settings_without_dx12(WgpuSettings::default());
            assert!(
                !settings
                    .instance_flags
                    .contains(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL)
            );
        });
    }

    /// `WGPU_VALIDATION_INDIRECT_CALL=1` still wins over the release-build removal.
    #[cfg(not(debug_assertions))]
    #[test]
    fn release_settings_honor_the_indirect_validation_override() {
        with_indirect_validation_env(Some("1"), || {
            let settings = settings_without_dx12(WgpuSettings::default());
            assert!(
                settings
                    .instance_flags
                    .contains(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL)
            );
        });
    }

    /// DX12 loses only `MULTI_DRAW_INDIRECT_COUNT`; every other feature is kept.
    #[test]
    fn dx12_removes_only_indirect_count() {
        let features = Features::all();
        assert_eq!(
            backend_features(Backend::Dx12, features),
            features - Features::MULTI_DRAW_INDIRECT_COUNT,
        );
        assert_eq!(
            backend_features(Backend::Dx12, Features::empty()),
            Features::empty()
        );
    }

    /// Non-DX12 backends keep their complete feature set.
    #[test]
    fn other_backends_keep_the_complete_feature_set() {
        let features = Features::all();
        for backend in [
            Backend::Vulkan,
            Backend::Metal,
            Backend::Gl,
            Backend::BrowserWebGpu,
        ] {
            assert_eq!(backend_features(backend, features), features);
        }
    }

    /// Bevy's explicit/disabled feature precedence still applies before the DX12 filter.
    #[test]
    fn explicit_and_disabled_features_retain_bevy_precedence() {
        let automatic = Features::TIMESTAMP_QUERY | Features::TEXTURE_COMPRESSION_BC;
        let disabled = Some(Features::TIMESTAMP_QUERY | Features::TEXTURE_COMPRESSION_BC);
        let explicit = Features::TEXTURE_COMPRESSION_BC | Features::MULTI_DRAW_INDIRECT_COUNT;
        // An ordinary explicitly requested feature still overrides the disabled
        // setting, while another disabled feature remains disabled.
        assert_eq!(
            configured_features(Backend::Vulkan, automatic, disabled, explicit),
            explicit,
        );
        // Even explicit COUNT cannot reintroduce the DX12 contract mismatch.
        assert_eq!(
            configured_features(Backend::Dx12, automatic, disabled, explicit),
            Features::TEXTURE_COMPRESSION_BC,
        );
        assert_eq!(
            configured_features(Backend::Vulkan, automatic, None, explicit),
            automatic | explicit,
        );
    }

    /// Exercises the real plugin/device path, including automatic backend selection.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires a GPU supporting indirect count on both DX12 and Vulkan"]
    fn selected_adapter_device_obeys_indirect_count_policy() {
        use super::RendererInitPlugin;
        use bevy::{
            app::TerminalCtrlCHandlerPlugin,
            audio::AudioPlugin,
            gilrs::GilrsPlugin,
            log::LogPlugin,
            prelude::*,
            render::{
                RenderPlugin,
                pipelined_rendering::PipelinedRenderingPlugin,
                renderer::{RenderAdapter, RenderAdapterInfo, RenderDevice},
                settings::{WgpuSettings, WgpuSettingsPriority},
            },
            window::WindowPlugin,
            winit::WinitPlugin,
        };

        for (backends, expected_backend) in [
            (wgpu::Backends::DX12, Some(Backend::Dx12)),
            (wgpu::Backends::VULKAN, Some(Backend::Vulkan)),
            (wgpu::Backends::all(), None),
        ] {
            let settings = WgpuSettings {
                backends: Some(backends),
                priority: WgpuSettingsPriority::Functionality,
                // Exercise an explicit COUNT request too: on DX12 the safety
                // policy must override it before the actual device request.
                features: Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
                    | Features::MULTI_DRAW_INDIRECT_COUNT,
                disabled_features: Some(Features::TIMESTAMP_QUERY),
                ..Default::default()
            };
            let mut app = App::new();
            app.add_plugins(
                DefaultPlugins
                    .build()
                    .disable::<RenderPlugin>()
                    .add_before::<RenderPlugin>(RendererInitPlugin(settings.clone()))
                    .disable::<WinitPlugin>()
                    .disable::<LogPlugin>()
                    .disable::<TerminalCtrlCHandlerPlugin>()
                    .disable::<AudioPlugin>()
                    .disable::<GilrsPlugin>()
                    .disable::<PipelinedRenderingPlugin>()
                    .set(WindowPlugin {
                        primary_window: None,
                        ..Default::default()
                    }),
            );
            app.finish();
            let info = app.world().resource::<RenderAdapterInfo>();
            let adapter = app.world().resource::<RenderAdapter>();
            let device = app.world().resource::<RenderDevice>();
            if let Some(expected_backend) = expected_backend {
                assert_eq!(info.backend, expected_backend);
            }
            // Requiring adapter support prevents a vacuous pass on a device
            // that could never expose the feature responsible for the bug.
            assert!(
                adapter
                    .features()
                    .contains(Features::MULTI_DRAW_INDIRECT_COUNT)
            );
            let mut expected = adapter.features();
            if info.device_type == wgpu::DeviceType::DiscreteGpu {
                expected.remove(Features::MAPPABLE_PRIMARY_BUFFERS);
            }
            expected.remove(Features::TIMESTAMP_QUERY);
            expected |= settings.features;
            if info.backend == Backend::Dx12 {
                expected.remove(Features::MULTI_DRAW_INDIRECT_COUNT);
            }
            assert_eq!(
                device.features(),
                expected,
                "selected backend: {:?}",
                info.backend
            );
        }
    }
}
