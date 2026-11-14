#!/usr/bin/env python3
"""Audit issue #467's pinned clustering sources without compiling or using a GPU.

The arithmetic cases demonstrate shader index hazards, not a reproduction of the
Riverwood lockup. Read cached crate archives in place; never extract or edit them.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile


FILES = {
    "bevy_pbr": [
        "src/cluster/cluster_allocate.wgsl",
        "src/cluster/cluster_z_slice.wgsl",
        "src/cluster/cluster_raster.wgsl",
        "src/cluster/gpu.rs",
        "src/cluster/mod.rs",
    ],
    "bevy_light": ["src/cluster/mod.rs"],
    "bevy_render": [
        "src/renderer/render_device.rs",
        "src/render_resource/pipeline_cache.rs",
        "src/render_resource/storage_buffer.rs",
    ],
    "bevy_shader": ["src/shader.rs"],
    "wgpu-hal": ["src/metal/device.rs"],
}


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def cached_sources(lock, registry):
    # A CRLF lockfile leaves a trailing carriage return before each line end,
    # so the anchored patterns below would never match. Normalise line endings;
    # callers still hash the raw bytes, so the lockfile digest is unchanged.
    lock = lock.replace("\r\n", "\n")
    sources, receipts = {}, []
    for name, paths in FILES.items():
        blocks = [block for block in lock.split("[[package]]")
                  if re.search(r'^name = "' + re.escape(name) + r'"$', block, re.M)]
        if len(blocks) != 1:
            raise ValueError("expected exactly one locked package: " + name)
        version = re.search(r'^version = "([^"]+)"$', blocks[0], re.M)[1]
        checksum = re.search(r'^checksum = "([^"]+)"$', blocks[0], re.M)[1]
        archives = list((registry / "cache").glob("*/" + name + "-" + version + ".crate"))
        if len(archives) != 1:
            raise ValueError("expected exactly one cached archive: " + name)
        archive = archives[0]
        if sha256(archive.read_bytes()) != checksum:
            raise ValueError("archive differs from Cargo.lock: " + str(archive))
        package = name + "-" + version
        with tarfile.open(archive, "r:gz") as contents:
            for relative in paths:
                data = contents.extractfile(package + "/" + relative).read()
                installed = registry / "src" / archive.parent.name / package / relative
                if installed.read_bytes() != data:
                    raise ValueError("installed source differs from locked archive: " + str(installed))
                key = name + "/" + relative
                sources[key] = data.decode()
                receipts.append({"source": str(installed), "sha256": sha256(data),
                                 "archive_sha256": checksum})
    return sources, receipts


def require(source, fragment):
    """Fail closed when a source premise changes; report its exact line."""
    if fragment not in source:
        raise ValueError("source premise changed: " + fragment)
    return source[:source.index(fragment)].count("\n") + 1


def audit(sources):
    allocate = sources["bevy_pbr/src/cluster/cluster_allocate.wgsl"]
    z_slice = sources["bevy_pbr/src/cluster/cluster_z_slice.wgsl"]
    raster = sources["bevy_pbr/src/cluster/cluster_raster.wgsl"]
    gpu = sources["bevy_pbr/src/cluster/gpu.rs"]
    premises = {
        "global_unconditional_access": require(allocate,
            "for (var i = 0u; i < cluster_count; i += 256u) {\n"
            "        offsets_and_counts.data[i + local_id.x][0].x += current_offset;\n"
            "        storageBarrier();"),
        "z_count_before_capacity_check": require(z_slice,
            "let z_slice_offset = atomicAdd(&cluster_metadata.indirect_draw_params.instance_count, 1u);\n"
            "    if (z_slice_offset >= cluster_metadata.z_slice_list_capacity) {\n"
            "        return;"),
        "unguarded_z_read": require(raster,
            "let instance_id = vertex.instance_id;\n"
            "    let object_index = z_slices[instance_id].object_index;"),
        "unclamped_indirect_draw": require(gpu,
            "render_pass.draw_indexed_indirect(cluster_metadata_buffer, 0);"),
        "logical_cluster_allocation": require(gpu,
            "view_clusters_bindings.reserve_clusters(cluster_count);"),
        "default_shader_checks_disabled": require(sources["bevy_shader/src/shader.rs"],
            "validate_shader: ValidateShader::Disabled"),
        "trusted_shader_checks_unchecked": require(sources["bevy_render/src/renderer/render_device.rs"],
            "create_shader_module_trusted(desc, wgpu::ShaderRuntimeChecks::unchecked())"),
        "disabled_shader_branch": require(sources["bevy_render/src/render_resource/pipeline_cache.rs"],
            "ValidateShader::Disabled => unsafe {\n"
            "            render_device.create_shader_module(module_descriptor)"),
        "metal_unchecked_policy": require(sources["wgpu-hal/src/metal/device.rs"],
            "let bounds_check_policy = if stage.module.bounds_checks.bounds_checks {\n"
            "                    naga::proc::BoundsCheckPolicy::Restrict\n"
            "                } else {\n"
            "                    naga::proc::BoundsCheckPolicy::Unchecked"),
    }
    tails = []
    for count in [1, 255, 256, 257, 3672, 4096]:
        # Enumerate the actual index expression; include aligned negative controls.
        accesses = [block + lane for block in range(0, count, 256) for lane in range(256)]
        invalid = [index for index in accesses if index >= count]
        guarded = [index for index in accesses if index < count]
        if guarded != list(range(count)):
            raise ValueError("guarded control failed")
        tails.append({"cluster_count": count, "invalid_lanes": len(invalid),
                      "first_invalid_index": invalid[0] if invalid else None,
                      "last_invalid_index": invalid[-1] if invalid else None,
                      "guarded_invalid_lanes": 0})
    slices = [{"capacity": 1024, "requested_slices": count,
               "written_slices": min(count, 1024), "draw_instances": count,
               "invalid_instance_reads": max(0, count - 1024)}
              for count in [1024, 1025, 1536, 3072]]
    return {"premise_lines": premises, "allocation_cases": tails, "z_slice_cases": slices,
            "limits": "CPU index arithmetic only; no GPU execution or Riverwood reproduction. "
                      "The failed session's cluster dimensions and shader were not captured."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--registry", type=Path, required=True, help="Cargo registry directory")
    parser.add_argument("--lockfile", type=Path, default=Path(__file__).resolve().parents[1] / "Cargo.lock")
    args = parser.parse_args()
    lock = args.lockfile.read_bytes()
    sources, receipts = cached_sources(lock.decode(), args.registry)
    print(json.dumps({"lockfile_sha256": sha256(lock), "sources": receipts,
                      "audit": audit(sources)}, indent=2))


if __name__ == "__main__":
    main()
