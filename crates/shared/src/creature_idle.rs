//! Explicit idle-only contracts for the two creature bodies verified in issue #471.
//! Paths are canonical archive/runtime keys. This is not behavior-graph discovery or retargeting.

#[derive(Debug, Clone, Copy)]
pub struct CreatureIdle {
    pub body_source: &'static str,
    pub body_scene: &'static str,
    pub skeleton_nif: &'static str,
    pub skeleton_hkx: &'static str,
    pub clip_source: &'static str,
    pub clip_scene: &'static str,
}

impl CreatureIdle {
    pub fn inputs(self) -> [&'static str; 4] {
        [
            self.body_source,
            self.skeleton_nif,
            self.skeleton_hkx,
            self.clip_source,
        ]
    }
}

pub const CREATURE_IDLES: [CreatureIdle; 2] = [
    CreatureIdle {
        body_source: "meshes/actors/cow/character assets/highlandcow.nif",
        body_scene: "meshes/actors/cow/character assets/highlandcow.glb",
        skeleton_nif: "meshes/actors/cow/character assets/skeleton.nif",
        skeleton_hkx: "meshes/actors/cow/character assets/skeleton.hkx",
        clip_source: "meshes/actors/cow/animations/mt_idle.hkx",
        clip_scene: "meshes/actors/cow/animations/mt_idle.glb",
    },
    CreatureIdle {
        body_source: "meshes/actors/ambient/chicken/character assets/chicken.nif",
        body_scene: "meshes/actors/ambient/chicken/character assets/chicken.glb",
        skeleton_nif: "meshes/actors/ambient/chicken/character assets/skeleton.nif",
        skeleton_hkx: "meshes/actors/ambient/chicken/character assets/skeleton.hkx",
        clip_source: "meshes/actors/ambient/chicken/animations/mt_idle.hkx",
        clip_scene: "meshes/actors/ambient/chicken/animations/mt_idle.glb",
    },
];

/// Matches a complete canonical body key, never a species substring or basename.
pub fn for_body_scene(scene: &str) -> Option<&'static CreatureIdle> {
    CREATURE_IDLES
        .iter()
        .find(|profile| profile.body_scene == scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_verified_complete_body_paths_select_an_idle() {
        for profile in CREATURE_IDLES {
            assert_eq!(
                for_body_scene(profile.body_scene).unwrap().clip_scene,
                profile.clip_scene
            );
            assert_eq!(
                profile
                    .inputs()
                    .into_iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                4
            );
        }
        for scene in [
            "chicken.glb",
            "meshes/actors/chicken/chicken.glb",
            "meshes/actors/custom/character assets/highlandcow.glb",
            "meshes/actors/character/composed/default/male/body.glb",
        ] {
            assert!(for_body_scene(scene).is_none(), "{scene}");
        }
    }
}
