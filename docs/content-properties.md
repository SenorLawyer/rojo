# Asset properties and Content

Roblox is moving asset references from their legacy properties to `Content` properties. From November 11, 2026, Studio saves places and models with the new properties, for example `Sound.AudioContent` instead of `Sound.SoundId` and `Decal.TextureContent` instead of `Decal.Texture`. See Roblox's [announcement](https://devforum.roblox.com/t/upcoming-changes-to-asset-properties-content-property-serialization-switch/4975546) for the full list.

Rojo reads both forms, so existing projects keep working and files Studio saves can be added to them.

## Writing asset properties

Either name works in project files, `.meta.json` and `.model.json` files:

```json
{
  "className": "Sound",
  "properties": {
    "SoundId": "rbxassetid://1234"
  }
}
```

```json
{
  "className": "Sound",
  "properties": {
    "AudioContent": "rbxassetid://1234"
  }
}
```

An empty string, `""`, sets a `Content` property to no content, which is how Studio saves an unset asset.

Rojo moves legacy properties to their `Content` replacements when it loads a project, the same way Studio and rbx-dom do when they open a file:

- Built places and models (`rojo build`) contain only the `Content` properties, which Studio reads.
- The plugin sets the `Content` property in Studio. Studio keeps the legacy property in step, so scripts that read `Sound.SoundId` still see the same asset.
- `CharacterMesh` ids such as `MeshId = 1234` become `rbxassetid://1234`, and `0` becomes no content.
- If an instance sets both names, the `Content` property is used and Rojo logs a warning when the two values differ.

Model files (`.rbxm` and `.rbxmx`) in either format are read as Studio reads them, including XML `Content` values written as `<uri>` or `<null />`.

## Syncback

`rojo syncback` compares a place saved by Studio with the project as Rojo loads it, so legacy names in your files count as equal to the `Content` properties in the place. A file is left as it is while its value matches. When a value changes, or Syncback creates a file for a new instance, it writes the `Content` property name.

Project files work the same way. A `$properties` entry with a legacy name stays as written while its value matches the place, and a project file whose entries all match is not rewritten. When Syncback rewrites a project file because something else in it changed, unchanged legacy entries keep their names. An entry whose value changed is replaced by the `Content` name, and an entry reset to its default is removed. Syncback does not add the `Content` name next to a legacy entry. If a node sets both names, the `Content` entry is updated and the legacy entry is removed unless it matches the place.

In `syncbackRules.ignoreProperties`, a legacy name and its `Content` name are the same property, so listing either one, such as `"Sound": ["SoundId"]` or `"Sound": ["AudioContent"]`, ignores both. Syncback leaves entries for an ignored property in project files as they are. A `.model.json` or `.meta.json` file is written from the place, so one that Syncback rewrites because something else in it changed loses them.

## Limits

- A `Content` value that refers to an object, such as an `EditableImage`, only exists in a running game. Rojo leaves such a property out of its tree and logs a warning. Syncback also logs a warning and handles the property as if `ignoreProperties` listed it for that instance, so it does not write the value and leaves the entry your files already have.
- `VideoPlayer.Asset` is not in Rojo's reflection database. Use `VideoPlayer.VideoContent`.
- `PluginToolbarButton.Icon` and `PluginToolbarButton.IconContent` belong to buttons that plugins create while Studio runs. Neither is saved in place or model files, and Rojo does not move one to the other.
