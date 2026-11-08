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
the base place itself. `--into` cannot be combined with `--plugin`.

When the output is the base itself, the base is checked again right before
it is replaced. If it changed since it was read, for example because Studio
saved it, the build stops without writing and leaves the newer file alone;
build again to apply the project onto that version.

`--watch` cannot write over its own base, since every rebuild would apply
the project onto the previous output. The paths are compared without either
file having to exist, following links and ignoring case on Windows, and
again before every rebuild.

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
  Setting a property also removes the base's other names for it: its
  aliases, and the properties it migrates to or from, such as `Color` when
  the project sets `BrickColor`, or `FontFace` when it sets `Font`.
  Otherwise the base's newer property would win when the place is written.
* Project instances without a pair are added.
* For base children that the project did not pair with anything,
  `$ignoreUnknownInstances` decides, as it does in Studio:
  * Where the project instance ignores unknown instances, the base's extra
    children are kept. This is the default for project nodes without a
    `$path`, so services listed only to set properties or to hold a few
    children keep everything else they have in the base. This includes
    children that share a name with a project child: of several base `Part`
    siblings, the project's `Part` pairs with the first and the others are
    left as they are, as the Studio plugin leaves them. A base child whose
    class differs from the project child of the same name stays next to it.
  * Where it does not, the project's content wins for that subtree and the
    base's extra children are removed. This is the default for a `$path`
    folder: a `Shared` folder built from `src/shared` contains exactly what
    the folder has, plus the base properties it does not set. To keep the
    base's extra children instead, set `"$ignoreUnknownInstances": true` on
    the node in the project file, or `"ignoreUnknownInstances": true`
    (without the `$`) in the folder's `init.meta.json`. A meta file does not
    recognize the `$` spelling, so the setting would have no effect.
* Services the project does not mention are left alone. They are never
  removed for lacking a project node, even when the root does not ignore
  unknown instances, since a place needs them. A top-level instance whose
  class the reflection database does not know is treated as a service.
  Other top-level instances, such as a folder or model directly under the
  DataModel, follow the root's setting like any other children.

A `$path` on `Workspace` that does not ignore unknown instances means the
project's content wins there too, which removes the base's map and
`Terrain`. Map content usually belongs under a node that ignores unknown
instances, for example:

```json
"Workspace": {
  "$path": "src/workspace",
  "$ignoreUnknownInstances": true
}
```

or `src/workspace/init.meta.json`:

```json
{ "ignoreUnknownInstances": true }
```

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
* A ref the project explicitly sets to nil, such as an `ObjectValue` in a
  model file with no value, clears the paired base instance's ref.
* A project `Rojo_Target_*` attribute can also point at a base-only instance
  that has a `Rojo_Id` attribute, such as one left by an earlier
  `rojo build`.
* `Rojo_Target_*` attributes the base place has are kept as data only and
  are not resolved. A ref the project sets, or one edited in Studio since the
  attribute was written, is what the output has.
* Project IDs (`$id`, `id` in meta files, `Rojo_Id` attributes) take
  precedence: when the project gives a paired instance an ID, its base
  `Rojo_Id` attribute is replaced with the project's ID.
* When the base place keeps a `Rojo_Id` that the project also gives one of
  its instances, neither could be resolved, so the build fails before writing
  anything and names both instances. Remove or rename the attribute in the
  base place, or change the project's ID.

## Watching

With `--watch`, the base place is checked for changes every half second as
well, and the output is rebuilt when it is saved. A build that failed because
the base was missing, unreadable or half written is retried the next time
the base changes, and the last good output stays in place meanwhile.

## Limits

* Everything in the base place goes through rbx-dom, as with `rojo
  syncback`, so content rbx-dom cannot read or write is not preserved.
* Instances are matched by name and class only. When the base has several
  siblings with a name the project defines, the first with the same class is
  paired; what happens to the others follows `$ignoreUnknownInstances`.
* Added instances come after the base instances under the same parent.
