# Merged terrain (#215)

Each exterior cell's terrain is one entity with one mesh, and every cell shares one terrain
material. Today the engine draws four quadrant entities per cell, each with its own material;
about 7 ms of render-thread time for a 17 x 17 ring is spent on that bookkeeping.

The converter writes the data (`shared::terrain_mesh`); this document is the engine side.

## Inputs

| File | Under the converted output root | What |
|---|---|---|
| Land texture table | `terrain/land-textures.json` (`LAND_TEXTURE_TABLE_PATH`) | The class arrays and the LTEX FormID to id table |
| Texture arrays | the table's `arrays[].path`, e.g. `terrain/land-diffuse-bc1-1024.ktx2` | One `texture_2d_array` per class (usage, block format, size), full mips |
| Cell chunks | `terrain/<wrld:08x>/chunk_<cx>_<cy>.rkyv` (`terrain_chunk_path`) | A `TerrainChunkFile`: the cell meshes of 4 x 4 cells |

A conversion without the table is `absent`: every cell takes today's quadrant path, with no
message per cell. A table that does not validate, a device that cannot bind it, an array file that
does not match the table, an array that fails to load or validate, or a setup that does not finish
within `SETUP_TIMEOUT` (120 s) is `refused`: one error line with the reason and the same fallback.
A table class outside the contract (a format or size `LandTextureTable::validate` does not list)
refuses merged terrain for the whole run, deliberately: the converter transcodes off-list formats
into the BC7 class, so a table that still names one is a converter fault, not content to work
around one cell at a time.

## Per cell

1. The streaming code asks `TerrainMerge::lookup` for the cell. A chunk file is opened once
   (memory mapped, validated by rkyv) and kept in a small cache; a missing chunk or cell entry is
   `NoData`, a cell that fails `TerrainCellMesh::validate` against the table is `Invalid`.
2. The cell's seam duplicates must agree and no normal may be zero; otherwise `Invalid`.
3. The cell's 33 x 33 height and normal grid goes through the same
   `validate_and_register_terrain_edges` the quadrant path uses (same bound, same corner rules,
   same recomputed normals), and the welded grid is written back to all 1,156 vertices. The
   registered edges are identical, so merged and quadrant cells weld onto each other. A merged
   cell whose weld fails is not failed: it is validated and welded by the quadrant path instead
   and counted as a fallback with the reason (`settle_cell_terrain`).
4. `build_merged_mesh` builds the mesh; one trimesh collider is built from the same buffers.
5. `NoData` and `Invalid` cells use the quadrant path, which is unchanged. Every cell with terrain
   is counted once: `terrain_cells_merged`, or `terrain_cells_fallback` (with
   `terrain_cells_fallback_expected_merged` and `terrain_cells_merged_invalid` for the cells whose
   merged data was expected or broken). The scene inventory counts split `scene/terrain_patches`
   into `scene/terrain_merged_cells` and `scene/terrain_quadrant_patches`.

While the arrays load, the responses of exterior cells that have terrain are held (in arrival
order, replayed first once setup ends) so a cell is never drawn by the quadrant path only because
the arrays were late. Interiors, cells with no LAND and failed loads are not held, and the first
hold is logged once. Setup is bounded: if no device verdict arrives (a run with no render world
posts its refusal at once; anything else waits out `SETUP_TIMEOUT`) or the arrays do not finish
loading, the merged path is refused and the held cells are drawn by quadrant. A held response
whose load has left the active space or the unload radius is stale: it is discarded at once,
counted as a stale response, and its outstanding slot is released. A held response still wanted
keeps its slot, so the held backlog stays within the streaming backlog limit.

Chunk files are memory-mapped whole and validated once with `rkyv::access`; the map starts on a
page boundary, which is the alignment rkyv needs, and no sub-slice of a chunk is read. A test reads
chunks produced by the converter's own writer.

A chunk file that does not exist is remembered as missing for the rest of the run: the converted
assets are fixed while the engine holds its lock on them, so a missing file stays missing and every
cell of a worldspace without merged data costs no further disk access.

## Mesh

Positions, normals, UVs and `COLOR_0` equal today's four quadrant meshes vertex for vertex,
stacked in quadrant order (4 x 289 vertices, 2,048 triangles, `u32` indices). Instead of the packed
tangent and second UV set the mesh carries the quadrant's slots:

| Attribute | Format | Locations |
|---|---|---|
| `TerrainSlotIds0..1` (`..3` at stride 16) | `Uint16x4`, land texture ids of slots 0-3, 4-7, ... | 8, 9 (10, 11) |
| `TerrainSlotWeights0..1` (`..3` at stride 16) | `Unorm8x4`, slot weights; slot 0 is stored as 0 | 12, 13 (14, 15) |

Ids are the table ids, mapped once from the file's LTEX FormIDs. The shader reconstructs slot 0 as
`max(0, 1 - sum of the others)` and blends as `terrain.wgsl` does. A stride of 16 is a second vertex
layout and so a second pipeline per pass.

## Material and shader

`MergedTerrainMaterial` binds up to nine diffuse and nine normal class arrays as static
`texture_2d_array` bindings (the contract's maximum: three block formats times three sizes per
usage), one sampler, the id-to-(class, layer) table as a storage buffer and the tiling constant.
`TerrainClassKey`, the material's bind group data, carries the number of classes in use, and
`specialize` turns it into shader defs, so a vanilla table compiles a shader that knows only its
six classes. The sampled array is data, so every sample is `textureSampleGrad` with derivatives of
the shared UV computed once.

Near and merged full terrain use the normal texture's alpha as a specular-intensity mask.
The shader blends it with the same paint weights and normalization as the surface colour,
then multiplies Bevy's dielectric reflectance by the square root of that mask. Since Bevy
uses `F0 = 0.16 * reflectance²`, this scales F0 linearly rather than squaring the mask.
A layer without a normal map contributes zero mask and its existing flat tangent normal;
that is a conservative engine fallback, not an established Skyrim missing-map default.
Normals without an alpha channel sample alpha as one.

This is a PBR approximation. Perceptual roughness remains 0.92, metallic remains zero,
and the base reflectance remains Bevy's 0.5 default before masking. Those are existing
engine choices, not Skyrim material constants. The normal sample supplies both its normal
and mask, without additional texture reads. Diffuse alpha is not used as the specular mask.

The material distinction follows the GPL-3.0 Community Shaders contributors'
[landscape normal-alpha blending and specular accumulation](https://github.com/community-shaders/skyrim-community-shaders/blob/e53c0a1ffaa01199490c6097a7dabb559932a88e/package/Shaders/Lighting.hlsl#L1192).
Skyrim's ordinary landscape path uses a Blinn-Phong lobe with a separately blended LTEX
SNAM exponent; this change does not implement that lighting function or infer roughness
from SNAM. The current runtime texture catalog does not expose that preserved raw field.
Partial masks scale F0, not the complete GGX lobe, and also affect Bevy's indirect energy
redistribution. Snow-specific rules, TXST specular flags, native lighting units and colour
domain, weather wetness, and distant LOD's specular suppression/fade remain outside this
bridge. It affects snow layers as well as dry ground and requires visual verification in
both. True distant LOD still uses its separate stock-material path.

Back faces match today's quadrant meshes: those carry tangents, so Bevy never flips their normal,
while a mesh without tangents has it flipped on a back face. The shader therefore reports every
fragment as a front face to Bevy's input builder, and terrain is lit by its interpolated normal
from both sides. The probe checks it on the GPU with a cell turned upside down under a sun.

The group-4 binding-array path is the later step. Its seam is the material's bindings and the two
`sample_*` helpers in `terrain_merge/terrain_merged.wgsl`.

## Limits

At startup the render world compares the device with the table: the class bindings plus an
allowance for Bevy's own textures against `max_sampled_textures_per_shader_stage`, every array's
layer count and the table's `max_layers_per_array` against `max_texture_array_layers`, and every
class size against `max_texture_dimension_2d`. Any excess refuses the merged path with the numbers
in the message.

## Array checks

Before anything loads, the first 80 bytes of every array file are read and the file's own
`vkFormat` must be the table's block format in the usage's colour space (an `_SRGB_BLOCK` format
for diffuse, a `_UNORM_BLOCK` one for normals), with the listed size and layer count. Bevy's KTX2
loader takes the colour space from the load settings, not from the file, so this header is the
only place a wrong-colour-space file can be caught. After loading, the image's descriptor is
checked for the block family, size, layer count, 2D-array view and repeating sampler. The arrays are
loaded as render-world-only assets, so the CPU copy of the pixels is dropped after upload; the
checks read the descriptor, which stays.

## Known differences from the quadrant path

- **Weights are interpolated per vertex.** Today the shader bilinearly interpolates the 17 x 17
  VTXT grid per fragment; the merged mesh interpolates per-vertex weights across each triangle.
  The two agree at the grid points and differ inside a quad, because a triangle interpolates
  barycentrically and a bilinear patch does not. Which one matches Skyrim's own interpolation is
  still to be checked on the render comparison.
- **More layers are drawn.** A quadrant with 7 to 16 layers is now drawn with all of them; today
  the cell fails (`quadrant_layers` refuses more than six). The converter report lists those cells.
- **`TerrainCellMesh.key` is not checked.** The engine cannot compute the derived-data key (it needs
  the plugin records), so a chunk that is stale after a LAND edit or a load order change draws as it
  is, as long as it validates against the table. The key (`converter::terrain_chunks::cell_mesh_key`)
  is a hash of the raw LAND, LTEX and TXST records and the declaring plugin's master list. The
  engine holds none of those at load (the world database and `cell_cache.rkyv` hold derived rows),
  so it cannot recompute the key, and comparing the stored key with one the converter also wrote
  could never fail. Follow-up for #185: reconversion compares keys and rewrites stale chunks, so a
  stale chunk never reaches the engine.

## Known costs

- Frustum culling is per cell rather than per quadrant.
- The cell cache snapshot is still read for the cell's water, and as the fallback.
