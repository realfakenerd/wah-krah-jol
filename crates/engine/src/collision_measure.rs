//! Opt-in instrumentation for the fixed-container comparison.

#[cfg(feature = "collision-measure")]
pub fn disabled() -> bool {
    std::env::var_os("MUDCRAB_COLLISION_MEASURE_OFF").is_some()
}
#[cfg(not(feature = "collision-measure"))]
pub fn disabled() -> bool {
    false
}

#[cfg(feature = "collision-measure")]
mod measurement {
    use crate::{
        app::{StreamingBenchmarkPhase, StreamingBenchmarkRoute},
        streaming::StreamingMetrics,
    };
    use bevy::prelude::*;
    use bevy_rapier3d::prelude::{PhysicsSet, ReadRapierContext};
    use std::{
        fs::File,
        io::{BufWriter, Write},
        time::Instant,
    };

    #[derive(Resource)]
    struct Samples {
        output: BufWriter<File>,
        start: Option<Instant>,
        step_ms: f64,
        steps: u64,
    }
    pub struct MeasurePlugin;
    impl Plugin for MeasurePlugin {
        fn build(&self, app: &mut App) {
            let path = std::env::var("MUDCRAB_COLLISION_MEASURE").expect("measurement output path");
            let mut output = BufWriter::new(File::create(path).expect("create measurement output"));
            writeln!(output, "phase,frame_ms,step_ms,steps,colliders,resident,loading,assets_ready,asset_failures,pending,authored").unwrap();
            app.insert_resource(Samples {
                output,
                start: None,
                step_ms: 0.0,
                steps: 0,
            })
            .add_systems(FixedUpdate, start.before(PhysicsSet::StepSimulation))
            .add_systems(FixedUpdate, finish.after(PhysicsSet::StepSimulation))
            .add_systems(Last, sample);
        }
    }
    fn start(mut samples: ResMut<Samples>) {
        samples.start = Some(Instant::now());
    }
    fn finish(mut samples: ResMut<Samples>) {
        if let Some(start) = samples.start.take() {
            samples.step_ms += start.elapsed().as_secs_f64() * 1000.0;
            samples.steps += 1;
        }
    }
    fn sample(
        time: Res<Time<Real>>,
        route: Option<Res<StreamingBenchmarkRoute>>,
        metrics: Res<StreamingMetrics>,
        physics: ReadRapierContext,
        mut samples: ResMut<Samples>,
    ) {
        let phase = route.as_ref().map_or(0, |r| {
            if r.phase == StreamingBenchmarkPhase::Tail {
                1
            } else {
                0
            }
        });
        let count = physics.single().map_or(0, |p| p.colliders.colliders.len());
        let step_ms = samples.step_ms;
        let steps = samples.steps;
        writeln!(
            samples.output,
            "{phase},{:.6},{step_ms:.6},{steps},{count},{},{},{},{},{},{}",
            time.delta_secs_f64() * 1000.0,
            metrics.resident_cells,
            metrics.loading_cells,
            metrics.assets_ready,
            metrics.asset_load_failures,
            metrics.pending_asset_instances,
            metrics.static_colliders_authored
        )
        .unwrap();
        samples.step_ms = 0.0;
        samples.steps = 0;
    }
}
#[cfg(feature = "collision-measure")]
pub use measurement::MeasurePlugin;
