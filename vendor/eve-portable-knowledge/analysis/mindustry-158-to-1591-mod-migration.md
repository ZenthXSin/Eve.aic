# Mindustry 158.1 → 159.1 Mod Migration Guide


## Executive Summary

159/159.1 is not just a balance update. For JVM mods it changes three foundations:

1. **Runtime baseline**: desktop/server Java bytecode target is now Java 17. Java 8 launch is dead.
2. **Data asset / data patch system**: maps/servers can inject content/assets. Content can now be removed/unapplied at runtime through `Vars.content.remove(content)`.
3. **Rendering pipeline**: static building rendering, LOD, and on-demand audio change assumptions around draw cost, cache invalidation, and resource lifetime.

Sharp summary: 159.1 kills the lazy “content id is forever, arrays are stable, static caches are harmless” style. Mods that resolve by name/type and rebuild caches after reload mostly survive.

## Version Anchor

- `v158`: removed deprecated unit ammo rule/system; max map size 800x800; logic and save fixes.
- `v158.1`: hotfixes for vault duplication, landing pad crash, status UI.
- `v159`: major data asset system, Java 17 target, LOD/static block rendering, lazy sound loading, new server asset transfer.
- `v159.1`: fixes around labels, map shrink crash, directional unloader sprite update, logic read crash, Android crash log export, and **data patches load after content**.

## 1. Build / Runtime Migration

Source: `build.gradle` diff.

- `sourceCompatibility` / `targetCompatibility` changed from Java 8 to Java 17 for desktop/server code.
- Server/client launch with Java 8 no longer works.
- Mod CI should compile/test with JDK 17.
- If using Kotlin, set JVM target to 17 and test against the new Mindustry artifacts.
- If using annotation processors or reflection touching JDK internals, Java 17 module boundaries may expose new failures. Avoid depending on `com.sun.*` unless necessary.

Migration checklist:

- Use JDK 17+ for Gradle and runtime tests.
- Remove assumptions that the game/server can run on Java 8.
- Rebuild release jars against 159.1 API, not only copy old jars forward.

## 2. Content IDs, Removal, and Hot Reload Risk

Sources:
- `core/src/mindustry/ctype/Content.java`
- `core/src/mindustry/core/ContentLoader.java`
- `core/src/mindustry/mod/DataPatcher.java`

Important source facts:

- `Content.id` is still assigned in constructor by current `Vars.content.getBy(getContentType()).size`.
- `Content` now has `public boolean removed`.
- `Content.removeContent()` was added as a hook.
- `ContentLoader.remove(@Nullable Content content)` removes content from the type `Seq`, `contentNameMap`, and global `nameMap`, then calls `content.removeContent()`.
- `DataPatcher.unapply()` marks patch content removed and removes added content.
- `DataPatcher.fixContentArrays()` resizes content-dependent arrays such as item/liquid filters through `Block.checkContentArrayCapacity(items, liquids)`.

Consequences:

- Removing content from `Seq` does **not** make old IDs a stable ABI. Existing `Content.id` values were assigned at construction time; later removal can create holes or invalid stale references.
- Static arrays keyed by `content.id` are now dangerous unless rebuilt after every data reload.
- Static references to patch-created content can survive in JVM mod classes even after Mindustry removed that content from `Vars.content`.
- Code that assumes `Vars.content.getBy(type).get(id) == contentWithThatId` is unsafe after data patch apply/unapply.

Migration pattern:

- Prefer `Vars.content.getByName(type, fullName)` for persistent references.
- Resolve lazily at use time, or keep a cache with explicit invalidation.
- Never persist raw `Content.id` as the only identity. Save `content.name` and `ContentType` instead.
- If you must use arrays by content ID for performance, rebuild them after data patch/content reload and guard `removed` content.
- In custom content subclasses, override `removeContent()` to dispose external resources and unregister side indexes.

## 3. Data Assets / Data Patches

Sources:
- `core/src/mindustry/mod/DataManager.java`
- `core/src/mindustry/mod/DataPatcher.java`
- `core/src/mindustry/mod/data/*`
- `core/src/mindustry/io/SaveVersion.java`
- `core/src/mindustry/net/Packets.java`, `NetServer.java`, `NetClient.java`

New system:

- Assets can be images, sounds, music, bundles, content JSON, and patches.
- Servers can provide assets from `assets/` beside the server jar.
- Save/map export can embed assets; otherwise external asset cache is involved.
- Networking has `AssetRequirementStream` and `AssetStream` for transferring required assets.
- Save format adds a `patches`/asset region before content/map/entity loading in newer versions.
- 159.1 explicitly says data patches load after content.

Mod impact:

- Data patch content behaves closer to mod content but is not the same lifecycle as normal JVM mod content.
- JSON/content assets can add content after base/mod content, causing content arrays and name maps to change after your mod initialized.
- Rules parsing may depend on data patches because `SaveVersion.readRules()` can be delayed until patches/assets are loaded.

Migration pattern:

- Do not build final content-dependent indexes only in static initializers.
- Rebuild indexes after content init and after data reload if your mod participates in client/server data assets.
- Treat patch content as volatile. Check `content.isPatchContent()` and `content.removed` where relevant.
- If your mod scans all blocks/items/liquids, do it from `Vars.content.getBy(type)` at the moment you need it, or maintain invalidatable caches.

## 4. Rendering Changes

Sources:
- `core/src/mindustry/core/Renderer.java`
- `core/src/mindustry/graphics/BlockRenderer.java`
- `core/src/mindustry/graphics/BuildingCacheLayer.java`
- `core/src/mindustry/graphics/Lod.java`
- `core/src/mindustry/world/Block.java`

New fields on `Block`:

- `drawDynamic = true`: if true, the building is drawn dynamically.
- `drawCached = false`: if true, `drawCached()` is used for cached building rendering.
- `buildingCacheLayer = BuildingCacheLayer.normal`.
- `drawBaseCached(Tile tile)` delegates to `Building.drawCached()`.

Block renderer now:

- Maintains chunked building sprite caches (`chunkSize = 30`).
- Tracks dirty chunks and recaches visible chunks.
- Has separate quadtree for cached blocks/buildings.
- Uses `Lod.update()` each frame.

LOD facts:

- `Lod.l1/l2` and `alpha1/alpha2` depend on screen scale.
- Screenshots temporarily set `Lod.disable = true`.
- Some details, e.g. status drawing, are skipped at lower LOD.

Migration pattern for rendering mods:

- If a building is visually static, consider `drawCached = true` and implement `Building.drawCached()` carefully.
- If animation/state changes affect cached visuals, call/trigger recache indirectly via tile change or avoid cached drawing.
- Do not mutate shader, texture, or draw state without restoring `Draw.reset()`/`Draw.z()`/batch state.
- Avoid `Draw.flush()` spam. It breaks batching and fights the new cache pipeline.
- Do not allocate `Texture`, `FrameBuffer`, `Shader`, `Pixmap`, `Vec2`, `Color` every frame.
- On reload/unload, dispose custom `Texture`, `FrameBuffer`, `Shader`, meshes and unregister post-draw hooks. 159 makes resource leaks more obvious because content/assets can reload.

Risk examples from local mod scan:

- `VoidShield` has custom `Shader`/`FrameBuffer` pipelines. It should add explicit reload/dispose and avoid stale shader maps across content reloads.
- `Factoriodustry` JS shader code creates `FrameBuffer`/`Shader` and custom `CacheLayer`; it should verify resize/dispose/reload behavior.
- `MindustryToolMod` creates `Texture` from downloaded pixmaps; these must be disposed/replaced safely.

## 5. Audio Changes

Sources:
- `core/src/mindustry/audio/SoundControl.java`
- `core/src/mindustry/audio/MusicContainer.java`
- Release note: sounds load when played instead of startup.

Changes:

- Lazy sound loading saves memory but changes timing assumptions.
- New map/server music support via `Rules.ambientMusic`, `Rules.darkMusic`, and logic/server remote methods.
- `SoundControl.findMusic(String)` and new ambient source handling exist.

Migration pattern:

- Do not assume all `Sound`/`Music` assets are loaded during client load.
- Resolve custom audio assets through the new asset path where possible.
- Avoid playing asset-backed music before assets are available.
- If using custom music rules, test both local map and server join path.

## 6. Save / Serialization Changes

Sources:
- `core/src/mindustry/io/SaveVersion.java`
- `core/src/mindustry/io/SaveOptions.java`
- `core/src/mindustry/io/JsonIO.java`

Changes:

- `SaveVersion.write()` now takes `SaveOptions`, not raw `StringMap` in the same way.
- Asset/data patch region is written/read.
- `JsonIO.read(Class<T>, T base, String string)` was replaced by `JsonIO.read(T base, String string)` that reads fields into an existing base object.
- `JsonIO.CustomJson` was removed.

Migration pattern:

- If your mod/plugin calls `JsonIO.read(SomeClass.class, existing, json)`, update it to `JsonIO.read(existing, json)`.
- If your code writes saves/maps directly, update to `SaveOptions` and decide whether assets must be embedded.
- For custom Building serialization, keep calling `super.write()` / `super.read()` and do not serialize raw runtime-only object references.

## 7. API Removals / Renames / Behavior Shifts

Observed source diffs:

- `core/src/mindustry/world/blocks/defense/DirectionalForceProjector.java` removed.
- `core/src/mindustry/world/blocks/units/DroneCenter.java` removed.
- `Rules.showSpawns` / `CampaignRules.showSpawns` changed to `hideSpawns` semantics.
- `ContentType` now carries `folderName`.
- `DrawPart.PartProgress` default methods now delegate to `CompatFix` for RoboVM compatibility; behavior intended unchanged.
- `TurretBuild` gained `isShooting`; shooting state code changed slightly.
- Deprecated unit ammo rule/system was already removed in 158, so mods still using it are broken before 159.

Migration pattern:

- Replace removed block subclasses or vendor your own implementation if you depended on them.
- Update rules UI/config code from `showSpawns` to `hideSpawns`.
- Recheck reflection against `ContentType` ordinal/name assumptions.
- Recompile: do not trust binary compatibility for internal classes.

## 8. Network / Server Plugin Impact

Sources:
- `NetServer.java`, `NetClient.java`, `Packets.java`, release notes.

Changes:

- Asset requirement and asset transfer streams added.
- Server world sending path now has `sendWorldAndAssets(Player)` / `sendAssetRequirements(Player)`.
- Snapshot serialization was optimized and refactored.
- `Call.label` performance improved; 159.1 fixes some label crashes.
- Dedicated server rules config moved to a separate file.

Migration pattern:

- Plugins that intercept world transfer, connection handshake, or custom packet flow must retest.
- Do not assume world stream is the only pre-join payload; assets may be required.
- If your plugin uses `debugStatusClient(... snapshotsSent)`, update signature; the third arg was removed.

## 9. Practical Migration Checklist

1. Build with JDK 17 and update Gradle/Kotlin JVM target.
2. Recompile against Mindustry 159.1, not 158 headers.
3. Grep your code for:
   - `Content.id`, `.id` arrays, `Vars.content.getBy(type).get(id)`
   - `JsonIO.read(` overload with base object
   - `showSpawns`, `DirectionalForceProjector`, `DroneCenter`
   - `FrameBuffer`, `Shader`, `Texture`, `Pixmap` allocation/disposal
   - static maps of content, static shader registries, custom cache layers
4. Replace ID persistence with `(ContentType, name)`.
5. Add cache rebuild hooks for content/data reload.
6. Dispose custom GL/audio resources on reload/unload.
7. Test in three paths:
   - fresh client launch
   - server join with assets
   - map/save load containing data assets/patches
8. For rendering blocks, decide `drawDynamic` vs `drawCached`; never cache animated state unless you explicitly recache.
9. Test zoomed-out rendering because LOD can hide details.
10. Run a headless server load test and a client visual smoke test.

## Severity Ranking

- Critical: Java 17 baseline; stale content ID/index caches; direct save/network API usage; removed internal classes.
- High: custom shader/framebuffer/texture lifecycle; server plugin world-transfer interception; content array assumptions.
- Medium: rules field rename, LOD visual omissions, lazy audio timing.
- Low: pure JSON/HJSON content that does not use removed fields or data patch edge cases.

## Bottom Line

159.1 is a compatibility tax on JVM mods that treated Mindustry internals as stable. The safe design is now: names over IDs, rebuildable caches over static snapshots, disposable GL resources over immortal singletons, and JDK 17 everywhere.

## Related

- [Mindustry Content Loading Chain](../concepts/mindustry-content-loading-chain.md)
- [Mindustry Rendering System](../mindustry/rendering-system.md)
- [Mindustry Save and Load System](../mindustry/save-load-system.md)
- [Mindustry Mod Lifecycle and Parser Boundary](../concepts/mindustry-mod-lifecycle-parser-boundary.md)
