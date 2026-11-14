# Riverwood fixed collision

The schema 4 `statics` table catalogs models for many record types. A fixed
collider is considered for placed `STAT`, `TREE`, and `FURN` records. Placed
`FURN` uses authored NIF collision only. `MISC`, `FLOR`, `DOOR`, and other
movable or interactive records remain outside this fixed-static path.

`ACTI` uses authored-only rest-pose collision: shapes owned exclusively by
dynamic bodies are omitted, while fixed, keyframed and unowned shapes retain
the existing fixed fallback. It receives no render-mesh proxy.

For `CONT`, the first supported subset is exactly `meshes/clutter/barrel01.glb`,
`meshes/clutter/tgbarrel01.glb`, `meshes/clutter/sovngarde/sovbarrel01.glb`, and
`meshes/clutter/sburialurn01.glb`. Admission requires version 2 body ownership,
no converter skips, and one massless `fixed` body on node 0, with a nonempty
target and layer 1 (`STATIC`), owning every shape. Missing bindings, unowned
shapes, multiple bodies, other layers, dynamic/keyframed bodies and version 1
shapes are skipped. Authored absence stays passable; missing collision extras
receive no proxy. Other container paths remain excluded and counted by the
authored-exclusion diagnostic. No schema change or reconversion is required
when the existing GLB satisfies this contract.

The four source NIFs in registered pack `user-testing-main-b5259d33` were
inspected read-only: each has one root collision body and no controller,
interpolator or constraint blocks. Its manifest SHA-256 is
`8a5e022024eae8c0abc1e3c3317c8f0135cd3f172175aa88d1e749df92d1d6a4`;
the owner's converter receipt identifies `b5259d33a73e1df730f0730866595fd4cdaae0e3`.
The inventory has 4,075 references to these models, including disabled references;
this is not a live collider count. Chests and other urns remain deferred:
`UpperChest01` has a controller-driven lid despite `fixed`/`STATIC` metadata.
This path policy does not establish arbitrary mod/script behavior. Source audit
and synthetic physics tests do not establish game-runtime or performance acceptance.

Converted GLBs now carry a versioned `mudcrabCollision` value in scene
extras. The converter follows each NIF `bhkCollisionObject` through its rigid
body and shape references. It decodes compressed meshes, capsules, boxes,
convex vertices, and NiTriStrips collision data. MOPP, list, and transform
wrappers are supported. Node, rigid-body, and compressed-chunk transforms are
applied before writing runtime coordinates.
The engine attaches a single authored shape to its fixed placement. For models
with multiple shapes, it attaches separate collider children to one fixed body.
Unloading the placement removes its colliders.
The converter cache schema is 16 so existing schema 15 GLBs are reconverted.
The runtime still accepts complete schema 15 packages through the legacy proxy
path.

NIF collision layers decide whether a shape is physical. The extractor accepts
`STATIC`, `ANIMSTATIC`, `TRANSPARENT`, `CLUTTER`, `TREES`, `PROPS`, `TERRAIN`, `GROUND`,
`INVISIBLE_WALL`, `STAIRHELPER`, and `COLLISIONBOX`; it leaves
`NONCOLLIDABLE` passable. An authored NIF with no collision object is also
passable. This is why clover, shrubs, and many small plants have no fixed
obstacle while tree trunks and buildings do. Other layers and unsupported
shape families are reported as skipped, not silently replaced with a box.

Older GLBs without the collision value retain the narrow render-triangle
proxy rule for known rock, architecture, pine, firewood, and road-ramp
families. The proxy's material exceptions for RockCliff faces and the
lumbermill walkway apply only to those older GLBs. An authored absence never
falls back to rendered triangles. Runtime metrics distinguish authored,
partial, absent, proxy, and skipped placements; warning logs name a skipped
model and reason once per model.

The Riverwood bridge and walkway stairs both contain authored compressed
collision meshes. The stair mesh separates ordinary `WOOD` support geometry
from `STAIRS_WOOD` geometry, but the public NIF schema does not establish that
Skyrim's player controller ignores either material. Both remain physical in
this implementation. Rapier autostep still needs enough clearance and landing
width to traverse each rise, so live stair traversal remains a manual gate.

The mill's visible log pile is a separate `FURN` placement, `MillLogPile.nif`,
with authored collision. Its collider is loaded with the mill cell. In
interactive Riverwood, toggle Rapier collision outlines through Graphics settings
or `tcg`. The status overlay reports whether they are on. The overlay draws actual physics shapes
that intersect the camera's forward half-space. Look toward a model before
using the outline to judge whether it has an active collider.

The collision extractor does not decode every Havok shape family. Its skip
reports are the coverage queue for those models. The Riverwood test area must
have zero unsupported fixed placements before treating its coverage as complete.

Version 2 collision extras add a `bodies` array with each `bhkRigidBody`'s
mass, inertia, centre of mass, damping, friction, restitution and velocity
limits. A placed reference whose base record type is movable (`MISC`, `WEAP`,
`ARMO`, `BOOK`, `AMMO`, `ALCH`, `INGR`, `SLGM`, `KEYM`, `SCRL`) and whose model
carries a body of kind `dynamic` becomes a dynamic Rapier body on the
reference entity, using the authored mass properties and falling and colliding
with the player, tankards and other clutter; a non-convex body uses one convex
hull instead. Fixed bodies on movable record types receive no collider. The fixed
tiers and audited container subset above are separate; other deferred types
(`FLOR`, `DOOR`, `MSTT` and the rest) keep no collider. A version 1 asset spawns
no new dynamic body, and the body despawns with its
cell. Two limits are deliberate for this slice: the character controller
cannot push dynamic bodies yet, and the authored `deactivator_type` is not
mapped, so bodies may sleep. Each body also starts asleep and stays where it
was placed until a contact (a tankard, other clutter), a grab or a set velocity
wakes it: models become ready one by one, so the shelf or terrain under a body
may not have its collider yet when the body appears. Clutter bodies use continuous collision detection,
the global linear speed cap is raised to 20,000 units/s and each body is clamped
to its authored limit. Only a model's first dynamic body is used (extra ones
are counted and logged), and over the live cap of 256 bodies a reference keeps
no collider until its cell loads again.

Dynamic clutter resolves each shape index once, preserving its first occurrence.
Repeated indices do not add collider entities or density-derived mass. Both collider
paths check a one-million-point geometry budget before construction. Bodies made
from convex primitives also have a separate cap of 256 collider parts; over-limit
bodies keep no collider and count as skipped. Non-convex bodies use one hull.
