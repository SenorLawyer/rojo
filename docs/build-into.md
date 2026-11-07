# Building into an existing place

`rojo build` writes a new place from the project, so anything that lives only
in the place file, such as the map, models, terrain and lighting settings, is
not in the output. `--into` applies the project onto an existing place
instead and leaves the rest of that place as it was:

```text
rojo build --into base.rbxl -o out.rbxl
rojo build --into place.rbxlx -o place.rbxlx
```

The base and the output can each be `.rbxl` or `.rbxlx`, in any combination.
The project's root must be a `DataModel`, as for any place build. The base
place is only read; the output is written the same way as any build, to a
temporary file that replaces the output once complete, so the output may be
the base place itself. `--into` cannot be combined with `--plugin`, and
`--watch` cannot write over its own base, since every rebuild would apply
the project onto the previous output.

## A complete place from a checkpoint

A checkpoint (`rojo checkpoint`) stores a saved place file. Export it and
build into it to get a place with both the checkpoint's map and the current
code:

```text
rojo checkpoint export before-update --output base.rbxl
rojo build --into base.rbxl -o release.rbxl
```

## What the project replaces

Instances are matched by path from the DataModel: a project instance pairs
with the first base instance under the matching parent that has the same name
and class, such as `ReplicatedStorage/Shared`. This is the matching
`rojo serve` uses against Studio.

* Paired instances stay in the base place and are updated: properties the
  project sets overwrite the base value, and properties it does not mention
  keep the base value. `Attributes` and `Tags` are each one property, so
  setting any attribute replaces all of that instance's attributes.
* Project instances without a pair are added.
* For base children that the project did not pair with anything,
  `$ignoreUnknownInstances` decides, as it does in Studio:
  * Where the project instance ignores unknown instances, the base's extra
    children are kept. This is the default for project nodes without a
    `$path`, so services listed only to set properties or to hold a few
    children keep everything else they have in the base.
  * Where it does not, the project's content wins for that subtree and the
    base's extra children are removed. This is the default for a `$path`
    folder: a `Shared` folder built from `src/shared` contains exactly what
    the folder has, plus the base properties it does not set. Set
    `"$ignoreUnknownInstances": true` on the node (or in `init.meta.json`) to
    keep the base's extra children instead.
* A base child with the same name as a project child is replaced even where
  unknown instances are ignored, for example a base `Remotes` model when the
  project defines a `Remotes` folder.
* Services the project does not mention are left alone. They are never
  removed for lacking a project node, even when the root does not ignore
  unknown instances, since a place needs them.

A `$path` on `Workspace` without `$ignoreUnknownInstances: true` means the
project's content wins there too, which removes the base's map and
`Terrain`. Map content usually belongs under a node that ignores unknown
instances.

## Refs

* Refs between base instances that both stay, such as an `ObjectValue` in the
  map pointing at another part of the map, keep pointing at the same
  instance. Paired instances keep their identity, so a base ref to an
  instance the project updates still points at it.
* Refs to a base instance the project removed or replaced are written as
  nil.
* Refs inside the project, from `.rbxm` or `.rbxmx` models or from
  `Rojo_Target_*` attributes, resolve to the instances in the output, whether
  they were paired with a base instance or added.
* A `Rojo_Target_*` attribute can also point at a base-only instance that has
  a `Rojo_Id` attribute, such as one left by an earlier `rojo build`.

## Limits

* Everything in the base place goes through rbx-dom, as with `rojo
  syncback`, so content rbx-dom cannot read or write is not preserved.
* Instances are matched by name and class only. When the base has several
  siblings with a name the project defines, the first with the same class is
  paired and the others are replaced.
* Added instances, including ones that replace a base instance, come after
  the base instances under the same parent.
