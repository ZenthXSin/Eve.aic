# Mindustry Data Patch vs JSON Mod

> Source: conversation + local Mindustry mods/examples

## Key Points

- `data patch` and JSON mod are different mechanisms.
- `data patch` is a reflection-style modification of an already existing object: it parses a patch string into `JsonValue`, resolves paths from the root (for example `blocks.xxx.health`), and assigns values onto existing fields.
- `data patch` can modify existing content fields and can append to arrays/Seq-like fields with `+`, but it is not a general content creation/loading system.
- JSON mod defines content. It creates new content based on file location and `type` (for example `content/blocks/production/a.json` with `type: GenericCrafter`).
- A normal `data patch` should not be treated as a replacement for JSON mod, and it should not be described as “able to add a mod/new block” in the general case.
- What can often be carried over from patch syntax: JSON/HJSON shape, dotted path semantics, array/Seq append behavior, and field updates on existing content.
- What must be treated carefully: creating new content registrations, overwriting original content through normal content JSON, and mixing internal patch field names with content-definition field names.
- If the goal is to convert patch output into JSON-mod-compatible content, the safe approach is to extract the final object state and write that as a content definition, instead of copying patch operators.

## Related

- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md) — content loading and post-processing context
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — content fields and parser-specific structures
- [Mindustry JSON 多液体输入](mindustry-json-multi-liquid-input.md) — example of structured JSON content fields
- [Mindustry Block 科技树 JSON 定义](mindustry-block-tech-tree-json.md) — block content and research hookup
- [Mindustry Placement/HUD UI 修改](mindustry-placement-ui-hud.md) — unrelated UI knowledge page for cross-reference
- [Mods Search Hub](../entities/mods-search-hub.md) — local mods/examples first-stop hub
- [Mindustry Reference Mod](../entities/mindustry-reference-mod.md) — local reference mod collection
- [CurseOfFlesh](../entities/curseofflesh.md) — local mod project with JSON examples
- [Helium](../entities/helium.md) — local mod project with UI examples
- [Mindustry JSON 技能约定](mindustry-json-skill-rules.md) — query priorities and JSON handling guidance
- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md) — loader and patch pipeline overview
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — field/interface summary for content JSON
- [Mindustry Block 科技树 JSON 定义](mindustry-block-tech-tree-json.md) — research/tree JSON structure
- [Mods Search Hub](../entities/mods-search-hub.md) — find local examples first
- [Mods File Index](../entities/mods-file-index.md) — file index of local examples

## Notes

This page intentionally separates “object patching” from “content definition” to avoid conflating `data patch` with JSON mod creation.
