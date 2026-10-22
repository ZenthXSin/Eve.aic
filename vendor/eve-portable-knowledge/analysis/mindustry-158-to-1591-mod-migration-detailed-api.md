# Mindustry 158.1 → 159.1 JVM Mod Detailed API Migration Notes


This page is the method/field/class-level companion to the summary guide. It focuses on API surfaces that can break JVM mods, plugins, custom renderers, save/network integrations, and reflection-heavy code.

## Reading Rules

- **Critical**: compile/runtime break or data corruption risk.
- **High**: likely visual/network/save bug, stale cache, or leak.
- **Medium**: behavior changed, retest needed.
- **Low**: mostly additive but useful to know.
- “Migration” means what a mod author should change, not what vanilla internally did.

## 1. Build and Runtime Baseline

### `build.gradle`

| Symbol | 158.1 | 159.1 | Severity | Migration |
|---|---:|---:|---|---|
| `sourceCompatibility` | Java 8 | `JavaVersion.VERSION_17` | Critical | Set Java/Kotlin target to 17 for desktop/server JVM mod builds. |
| `targetCompatibility` | Java 8 | `JavaVersion.VERSION_17` | Critical | Do not ship Java 8 bytecode assumptions as your only test path. |

Impact:
- Java 8 runtime is dead for current desktop/server paths.
- Kotlin mods should set `kotlinOptions.jvmTarget = "17"` or toolchain 17.
- Reflection against JDK internals may fail under Java 17 module behavior. If a mod uses `Unsafe`, classloader hacks, or `com.sun.*`, retest instead of guessing.

## 2. Content Identity, Removal, and Patch Volatility

### `mindustry.ctype.Content`

| Symbol | Type | Change | Severity | Migration |
|---|---|---|---|---|
| `public short id` | field | Still assigned in constructor from `Vars.content.getBy(getContentType()).size` | Critical | Do not treat `id` as stable after data patch load/unload. |
| `public ModContentInfo minfo` | field | Existing field now may include `asset` | Medium | If inspecting `minfo`, handle patch assets. |
| `public boolean removed` | new field | Marks content invalid after data patch removal | Critical | Guard cached content references with `content.removed`. |
| `public void removeContent()` | new method | Hook called by `ContentLoader.remove()` | High | Override in custom content that owns resources/indexes; call `super`. |
| `public boolean isPatchContent()` | new method | True when `minfo.mod == DataPatcher.dpMod` | High | Use to identify volatile data-patch content. |
| `ModContentInfo.asset` | new field | Associated `ContentAsset` for save/data-loaded content | Medium | Useful for diagnostics; null for normal mod content. |

Direct source fact: the constructor still does `this.id = (short)Vars.content.getBy(getContentType()).size;`. Removal does not reconstruct every existing content object. So static arrays indexed by `content.id` become unsafe after patch content is inserted/removed.

Migration pattern:
- Persist `(ContentType, content.name)`, not raw `id`.
- Resolve with `Vars.content.getByName(type, name)` at load/use time.
- If performance needs ID arrays, rebuild them after content/data reload and size-check every access.
- For subclasses with GL/native resources, override `removeContent()` to dispose/unregister.

### `mindustry.core.ContentLoader`

| Symbol | Type | Change | Severity | Migration |
|---|---|---|---|---|
| `public void remove(@Nullable Content content)` | new method | Removes from typed `Seq`, `contentNameMap`, global `nameMap`, then calls `content.removeContent()` | Critical | JVM mods must assume content can disappear from loader maps at runtime. |
| duplicate-name exception text | behavior | Message changed | Low | Do not parse exception text. |

Risk:
- `Vars.content.getBy(type)` can shrink.
- `Vars.content.getBy(type).get(id)` is no longer safe identity lookup under patch churn.
- `contentNameMap` lookup may return null for a cached stale object name if removed.

### `mindustry.mod.DataPatcher`

| Symbol | Type | Change | Severity | Migration |
|---|---|---|---|---|
| `public static final LoadedMod dpMod` | new field | Synthetic mod identity for data patches | High | Patch content reports `isPatchContent()`. |
| `public static final int maxImageSize = 2000` | new field | Data image size bound | Medium | Generated map/server images above this fail. |
| `public static final int patchFormatVersion = 2` | changed/added | Save/network patch format anchor | High | Custom save readers must handle v2 data assets. |
| `private static boolean needsArrayFix` | new field | Tracks content array resize need | High | Indicates vanilla itself expects arrays to become stale. |
| `apply(Seq<String>)` | removed | Old raw patch string API gone | Critical | Use `apply(Seq<PatchAsset>, Seq<ContentAsset>)`. |
| `public void apply(Seq<PatchAsset> patches, Seq<ContentAsset> content)` | new method | Applies data patch/content assets | Critical | Plugins calling old patch API must update. |
| `public void apply(..., boolean reloadContentWorld)` | new method | Controls content-world reload/array fixing | High | If false, caller must know array consequences. |
| `public void unapply(boolean reloadContentWorld)` | new method | Removes added content and vars | Critical | Stale mod caches must be invalidated after this. |
| `public Seq<Content> getAddedContent()` | new method | Exposes patch-created content | Medium | Useful for detecting volatile content. |
| `public static void fixContentArrays()` | new method | Resizes content-indexed arrays/modules | Critical | Mirrors what mods should do for their own ID arrays. |
| `PatchSet` nested class | removed | Replaced by `PatchAsset` | Critical | Update imports/reflection. |

Internal behavior worth caring about:
- Added content is tracked in `addedContent`.
- Logic vars added by patches are tracked in `addedVars` and removed on unapply.
- `fixContentArrays()` walks blocks and buildings to resize item/liquid arrays.

### `mindustry.mod.DataManager`

New central owner for data assets and patch lifecycle.

| Symbol | Type | Severity | Migration |
|---|---|---|---|
| `reloadContent(boolean reloadArrays)` | method | Critical | If a mod hooks reloads, rebuild content caches after this, not before. |
| `regenerateContentSprites(boolean forcePack)` | method | High | Generated sprites can change atlas-derived regions; stale `TextureRegion` caches can lie. |
| `reloadPatches(Seq<PatchAsset>)` | method | High | Patch changes imply content identity/index changes. |
| `reloadImages()` / `reloadImages(Seq<ImageAsset>)` | method | High | Custom atlas/region references need refresh checks. |
| `reloadAudio()` | method | Medium | Audio assets may appear after map/server data load. |
| `load(Seq<DataAsset>)` | method | Critical | Loads all data asset classes; patch/content load happens here. |
| `unload()` | method | Critical | GL/audio/content resources can disappear; dispose your own matching state. |
| `getMissingAssets()` | method | High | Server join/save load can require external cache. |
| `getAllAssets()` / `getAllExternalAssets()` | method | Medium | Save/export/embed decisions. |
| `hasAssetPath/Name/NameOrPath(...)` | methods | Low | Prefer these over manual asset seq scans. |
| `isPatched(Object content)` | method | Medium | Replacement for old `state.patcher.isPatched(content)` UI checks. |
| `getImages/getPatches/getMusic/getSounds/getBundles/getContent()` | methods | Medium | Use typed accessors, but don't mutate without understanding reload order. |

## 3. Data Asset Classes

### `mindustry.mod.data.DataAssetType`

New enum values:
- `patch("patches", json/hjson/json5, PatchAsset, embedded=true)`
- `content("content", json/hjson/json5, ContentAsset, embedded=true)`
- `bundle("bundles", properties, BundleAsset, embedded=false)`
- `image("sprites", png, ImageAsset, embedded=false)`
- `sound("sounds", mp3/ogg, SoundAsset, embedded=false)`
- `music("music", mp3/ogg, MusicAsset, embedded=false)`

Fields/methods:
- `folder`, `extensions`, `constructor`, `embedded`, `all`
- `create()`
- `localized()`

Migration:
- Treat content/patch assets as always embedded and volatile.
- Treat image/audio/bundle as potentially external and cache-backed.

### `DataAsset`

Important fields/methods:
- `path`, `name`
- `byteHash`, `stringHash`
- `updateData(byte[])`, `setHash(byte[])`, `setPath(String)`
- `getFullPath()`, `getData()`
- `isAlwaysEmbedded()`, `isCached()`, `getCacheFile()`, `getCacheFileNoNull()`
- `readFromZip(...)`, `readOverride(...)`, `read(...)`, `write(...)`

Migration:
- If plugin/tool exports saves/maps, preserve hash vs embedded distinction.
- Do not assume `getData()` works for every asset; base implementation throws.

### `ContentAsset`

Important fields/methods:
- `static ContentType[] loadableContent = {item, block, liquid, status, unit, weather}`
- `ContentType type`
- `String data`
- `Seq<String> warnings`
- `@Nullable Content content`
- `boolean errored`
- `hashData()`
- `readOverride(String path, Fi file, ContentType type)`
- `getFullPath()` returns `content/<folderName>/<path>`

Migration:
- Data content does not cover every `ContentType`; only listed types are loadable.
- Code using `ContentType` folder names should use the new `folderName` field.

### `PatchAsset`

Fields/methods:
- `String patch`
- `JsonValue json`
- `boolean error`
- `Seq<String> warnings`
- `readFromZip`, `readOverride`, `read`, `write`, `getData`, `toString`

Migration:
- Old `DataPatcher.PatchSet` references must become `PatchAsset`.

### `ImageAsset`, `SoundAsset`, `MusicAsset`, `BundleAsset`

Migration highlights:
- `ImageAsset.isGenerated()` exists; generated images may be cleared/repacked.
- `BundleAsset.cachedBundle` and `tryLoadCache()` means bundle data may be lazily cached.
- Sound/music are first-class data assets; don't assume startup-only asset loading.

## 4. Rendering Pipeline: Static Building Cache and LOD

### `mindustry.world.Block`

| Symbol | Type | Change | Severity | Migration |
|---|---|---|---|---|
| `public boolean drawDynamic = true` | new field | Controls dynamic draw participation | High | Set false only when dynamic overlay is unnecessary or separately handled. |
| `public boolean drawCached = false` | new field | Enables cached building rendering | High | Static visuals can opt in; animated/stateful visuals need recache. |
| `public BuildingCacheLayer buildingCacheLayer = BuildingCacheLayer.normal` | new field | Cache layer selection | Medium | Use `under` for underlay-style cached drawing. |
| `public void drawBaseCached(Tile tile)` | new method | Delegates to `tile.build.drawCached()` | High | Override `Building.drawCached()` for cached visuals. |
| `public void checkContentArrayCapacity(int items, int liquids)` | new method | Resizes `itemFilter/liquidFilter` | Critical | Custom blocks with content-indexed arrays should override and resize. |

Migration:
- For static blocks: set `drawCached=true`, implement `Building.drawCached()`, call `recache()` when config/proximity/state changes visual output.
- For animated blocks: leave `drawDynamic=true`; don't cache animation frames unless invalidation is exact.
- If you keep arrays sized by `content.items().size` or `content.liquids().size`, add your own resize hook equivalent to `checkContentArrayCapacity`.

### `mindustry.graphics.BlockRenderer`

New fields/classes/methods:
- Constants: `chunkSize = 30`, `maxSpritesPerCacheTile = 3`, `blockDrawCountDebug`
- Cached tree: `blockCachedTree`
- Cache arrays: `CacheChunk[][] cacheChunks`, `Seq<SpriteCache>[] caches`, dirty chunk tracking
- `public void recacheBuilding(BuildingCacheLayer layer, Tile tile)`
- `public void cacheChunk(int layer, int cx, int cy)`
- `static class CacheChunk`
- `indexBlockCached(Tile)` internal predicate

Behavior changes:
- Visible cached chunks are recorded and drawn as `SpriteCache` batches.
- Dirty chunks are recached only when visible or queued.
- Team visibility/fog affects cached output; cache tracks `lastSeenTeam`.
- Status drawing moved to a separate pass and is gated by `Lod.l2`.
- `BlockRenderer.reload()` disposes old sprite caches.

Migration:
- Custom buildings that visually change due to config/proximity must call `recache()` or otherwise trigger vanilla recache. Many vanilla blocks now call `recache()` in `configured`, `onProximityUpdate`, etc.
- Avoid calling `Draw.flush()` inside normal block draw. Cache building internally flushes; extra flushes kill batching.
- Do not mutate global draw state without `Draw.reset()`/restoring `Draw.z`.
- Reflection into `BlockRenderer` internals is now brittle; names and data structures changed.

### `mindustry.graphics.BuildingCacheLayer`

New enum:
- `under(Layer.block - 0.5f)`
- `normal(Layer.block)`
- static `all`, `amount`, `layers`

Migration:
- Use this instead of abusing `CacheLayer` for building sprites.

### `mindustry.graphics.Lod`

New class:
- `disable`
- `l1`, `alpha1`
- `l2`, `alpha2`
- `update()` computes levels from `graphics.getWidth() / camera.width`

Migration:
- Expensive details should obey `Lod.l1/l2` if you want vanilla-style performance.
- If your visual carries gameplay-critical information, do not hide it solely behind LOD without fallback.
- Screenshot path may set `Lod.disable = true`, so visuals can differ during screenshot/export.

### Vanilla block examples that changed

| Class | Changed symbols | Migration lesson |
|---|---|---|
| `MendProjector` | `draw()` split into `drawCached()` + dynamic `draw()`; dynamic uses `Lod.l2` | Separate static base from expensive overlay. |
| `OverdriveProjector` | overlay gated by `Lod.l2` | Zoomed-out details may vanish. |
| `Thruster` | `drawCached()` added | Static block visuals moved into cache. |
| `Wall` | `autotile`, `autotileRegions`, `updateAutotileBits()`, `updateOtherBits()`, recache hooks | Neighbor-dependent cached blocks must recache neighbors. |
| `Duct` / `Conduit` | `draw(boolean under)`, `drawCached()`, `drawAt(..., boolean under)`, `recache()` calls | Cached underlay + dynamic item/liquid overlay split. |
| `StackConveyor` / `StackRouter` / bridges / power nodes | `drawCached()` added | Transport blocks increasingly use cache for static pieces. |

## 5. Save / Load / Serialization

### `mindustry.io.SaveVersion`

| Symbol | 158.1 | 159.1 | Severity | Migration |
|---|---|---|---|---|
| `write(DataOutputStream)` | final overload existed | removed/changed via writer API | Critical | Use writer API with `SaveOptions`. |
| `write(DataOutputStream, StringMap extraTags)` | public | `write(DataOutputStream, SaveOptions options)` | Critical | Replace raw tags overload with `SaveOptions.extraTags`. |
| `read(..., WorldContext context)` | direct context | `read(..., SaveReadState saveState)` | Critical | Custom save readers must pass `SaveReadState`. |
| `readMeta(DataInput, WorldContext)` | direct rules parse | `readMeta(DataInput, SaveReadState)` | High | Rules can be delayed until data patches load. |
| `readRules(SaveReadState)` | new | parses saved rules after patch availability | High | If rules reference patch content, parse after assets. |
| `skipContentPatches(...)` | removed | replaced by data asset region | Critical | Old patch chunk assumptions invalid. |
| `readContentPatches(...)` | removed | `readDataPatches(...)` | Critical | Update custom readers/tools. |
| `writeContentPatches(...)` | removed | `writeDataPatches(DataOutput, boolean forceEmbed)` | Critical | Update save/map export code. |
| `writeEntity(...)` | new private helper | units explicitly written from `Groups.unit` too | Medium | Entity serialization path changed; retest custom entities. |

New save order for v12+ includes a `patches`/data asset region before content/map/entities. For v13+, rules parsing is explicitly delayed until patches are loaded.

### `mindustry.io.SaveOptions`

New class:
- `public boolean embedAssets`
- `public @Nullable StringMap extraTags`

Migration:
- For export/share: set `embedAssets=true` when the save/map must be standalone.
- For local saves: external assets may be hash-backed.

### `mindustry.io.SaveReadState`

New class:
- `public final WorldContext context`
- `public @Nullable String ruleString`

Migration:
- Custom readers should preserve rule string until data patches are loaded.

### `mindustry.io.JsonIO`

| Symbol | Change | Severity | Migration |
|---|---|---|---|
| `read(Class<T> type, T base, String string)` | removed | Critical | Replace with `read(T base, String string)`. |
| `read(T base, String string)` | new | Critical | It calls `json.readFields(base, parsed)`. |
| `CustomJson` nested class | removed | High | Reflection/imports break. |
| `MusicContainer` serializer | added | Medium | Rules music serializes as string names. |
| `classTag(String, Class<?>)` | no longer updates `jsonBase` | Low | Only one json instance. |

Migration example in words:
- Old: “read JSON into this existing object by passing class + base”
- New: “pass the base object only; type comes from object instance”

## 6. Network / Server Plugin Surface

### `mindustry.net.Net`

Changed/additions:
- `packetIdAssetStream`, `packetIdWorldStream`
- `registerPacket(Prov<T>)` now returns packet id
- `getPacketClassId(Class<?>)`
- `connect(...)` clears streams/currentStream before connecting
- `disconnect()` closes open stream builders
- `send(Object, Iterable<NetConnection>, boolean)` bulk-send API
- provider defaults: `sendAllServer`, `sendExceptServer`
- stream begin now knows whether target stream is incremental

Migration:
- Plugins registering packets should not assume `registerPacket` is void.
- Stream listeners must handle asset streams before/alongside world streams.
- If intercepting world send, handle `AssetRequirementStream` and `AssetStream`.

### `mindustry.net.Packets`

New classes:
- `AssetRequirementStream extends Streamable`
- `AssetStream extends Streamable`, overrides `incremental() = true`

Changed:
- `StreamBegin.incrementalStream` transient input stream for incremental handling.

Migration:
- Asset stream is consumed incrementally; do not wait for a built byte array event.

### `mindustry.net.Streamable`

New API:
- `public boolean incremental()` default false
- `StreamBuilder(StreamBegin, boolean incremental)`
- `StreamBuilder.incremental`, `incrementalStream`, `received`
- `IncrementalStream` nested input stream, can be closed/finished

Migration:
- Custom streamable packets should explicitly decide if incremental behavior is safe.
- If you consume current stream progress, use `received/total`, not byte array size.

### `mindustry.net.NetworkIO`

New methods:
- `writeRequiredAssets(OutputStream, Seq<DataAsset>)`
- `readRequiredAssets(InputStream)`
- `writeAssets(OutputStream, Seq<DataAsset>)`
- `loadAssets(InputStream)`

Changed world serialization:
- Data patches/assets are written before rules because rules may reference patched content.
- Old content patch read/write calls are removed.

Migration:
- Server plugins that send worlds manually must send/handle required assets first.
- Client-side join logic must allow asset cache population before world read.

### `mindustry.core.NetServer` / `NetClient`

Observed API/behavior themes from diff:
- Server world send path now distinguishes asset requirements/assets/world stream.
- Connections have asset-determination states.
- Handshake path can include asset requirements before world data.

Migration:
- Retest plugins touching `sendWorld`, connect lifecycle, or stream transfer.
- Do not assume `WorldStream` is the first/only large pre-join stream.

## 7. Rules / Gameplay Data Fields

### `mindustry.game.Rules`

| Symbol | Change | Severity | Migration |
|---|---|---|---|
| `showSpawns` | removed/renamed semantic | Critical | Replace with `hideSpawns`; logic is inverted. |
| `hideSpawns = true` | new field | Critical | Old config UI/scripts need inversion. |
| `unitFactoryActivationDelay` | new field | Medium | Unit factories may be delayed globally. |
| `ambientMusic` | new nullable `Seq<MusicContainer>` | Medium | Rule JSON can override music via names. |
| `darkMusic` | new nullable `Seq<MusicContainer>` | Medium | Same for dark/boss music. |
| `alwaysPlayMusic` | new field | Low/Medium | Overrides music setting behavior. |
| `retainContentFields(Rules source)` | new method | High | Preserves patch-content fields when custom rules are applied. |
| `unitActivationDelay(Team team)` | new method | Medium | Use instead of reading only global/team field. |

### `mindustry.game.CampaignRules`

| Symbol | Change | Migration |
|---|---|---|
| `showSpawns` | replaced by `hideSpawns` | Update JSON/reflection/UI bindings and invert meaning. |

## 8. Audio API

### `mindustry.audio.MusicContainer`

New lazy wrapper:
- `String name`
- `Music get()` lazily calls `SoundControl.findMusic(name)` and caches result

Migration:
- Store music names in serialized rules; resolve lazily after assets load.

### `mindustry.audio.AmbientSource`

New interface:
- `boolean isValid()`
- `boolean shouldAmbientSound()`
- `float getAmbientVolume()`
- `Sound getAmbientSound()`
- inherits `Position`

Migration:
- Custom ambient sources can register through `SoundControl.addAmbientSource(source)`.
- Source methods may run on an audio helper thread; avoid touching unsafe game state without care.

### `mindustry.audio.SoundControl`

Important additions/changes:
- `keepSilent()`
- `playMusic(@Nullable Music music, boolean interrupt)`
- `isPlaying()`
- `alwaysPlayMusic()` respects rules/planet/settings
- `getBossMusic()`, `getAmbientMusic()`, `getDarkMusic()` resolve rule/planet/default music
- `findMusic(String name)` searches asset names and `music/<name>.ogg/.mp3`
- `addAmbientSource(AmbientSource)` starts/manages `AudioThread`
- loop system moved into reusable static `loop(SoundData, Sound, Position, volume, pitch)` and `SoundData` now stores `sumX/sumY`, implements `Cloneable`

Migration:
- Do not assume all music assets exist at client load; map/server assets can add later.
- Avoid direct mutation of `SoundControl.sounds` internals; shape changed.
- Ambient sound callbacks must be thread-tolerant.

## 9. Content Arrays and Modules

### `mindustry.world.modules.ItemModule`

Changes:
- `windowSize: 6 → 12`
- `pollScl: 20f → 10f`
- flow update interval `30 → 15`
- `has(ItemStack[], float)` now guards `stack.item.id >= items.length`
- `checkArrayCapacity(int size)` added

Migration:
- If custom modules mirror item arrays, add size guards.
- Flow display timing changed; visual/stat tests may differ.

### `mindustry.world.modules.LiquidModule`

Changes:
- `windowSize: 3 → 6`
- `pollScl: 20f → 10f`
- flow update interval `30 → 15`
- `checkArrayCapacity(int size)` added

Migration:
- Same: resize arrays after patch content changes.

### Vanilla content-array overrides

Classes adding `checkContentArrayCapacity(int items, int liquids)`:
- `Block`
- `Accelerator`
- `Reconstructor`
- `UnitAssembler`
- `UnitFactory`

Migration:
- Any custom block with arrays indexed by item/liquid ID should implement the same pattern.

## 10. Removed / Renamed Classes

### Removed: `mindustry.world.blocks.defense.DirectionalForceProjector`

Fields/methods removed with the class included:
- `width`, `shieldHealth`, `cooldownNormal`, `cooldownLiquid`, `cooldownBrokenBase`
- `absorbEffect`, `shieldBreakEffect`, `topRegion`, `length`, `padSize`
- `init()`, `setBars()`, `outputsItems()`, `setStats()`, `drawPlace(...)`
- inner `DirectionalForceProjectorBuild` with `broken`, `buildup`, `hit`, `warmup`, `shieldRadius`, `updateTile()`, `deflectBullets()`, `drawShield()`, `write/read`

Migration:
- If imported directly, your mod no longer compiles.
- Vendor your own copy or migrate to `BaseShield`/`ShieldWall`-style code if behavior is needed.

### Removed: `mindustry.world.blocks.units.DroneCenter`

Fields/methods removed:
- `unitsSpawned`, `droneType`, `status`, `droneConstructTime`, `statusDuration`, `droneRange`
- inner `DroneCenterBuild`: `readUnits`, `readTarget`, `units`, `target`, `droneProgress`, `droneWarmup`, `totalDroneProgress`, `updateTile()`, `drawConfigure()`, `draw()`, `write/read`
- inner `EffectDroneAI.updateUnit()`

Migration:
- Direct subclasses/imports break.
- Recreate behavior in your mod or redesign using current unit/ability APIs.

### `mindustry.world.draw.DrawBlock`

Change:
- Was `abstract class DrawBlock`, now concrete `class DrawBlock`.

Migration:
- Mostly source-compatible for subclasses, but reflection/tests expecting abstract fail.

## 11. Turret / Logic / Unit Factory Details

### `mindustry.world.blocks.defense.turrets.Turret.TurretBuild`

Changes:
- `wasShooting` becomes `wasShooting, isShooting`
- new `controlled()` method wraps control logic
- call sites changed from `isControlled()` to `controlled()` in some shooting behavior

Migration:
- Mods overriding turret shooting should inspect `isShooting` field behavior.
- Avoid storing your own duplicate “was shooting” state unless needed.

### `ContinuousTurret`

Change:
- Uses `isShooting` field rather than repeatedly calling `isShooting()` in key paths.

Migration:
- If subclass overrides `isShooting()` expecting all callers to hit it, retest. Some paths now read field state.

### `LogicBlock`

Important additions:
- `@Nullable ObjectIntMap<String> linkMap`
- `optionalLink(String name)` helper
- guards for `linksVar == null`

Migration:
- Logic link resolution was hardened; custom logic extensions should not assume link map exists.

### Unit factory blocks

Classes `Reconstructor`, `UnitAssembler`, `UnitFactory` now return `BlockStatus.inactiveUnitFactory` when team unit factories are delayed/inactive.

Migration:
- UI/status mods should recognize new `BlockStatus.inactiveUnitFactory`.

## 12. UI / Platform API Surface With Mod Impact

### `mindustry.ui.FileChooser`

New central API:
- `export(String name, String extension, FileWriter writer)`
- `open(String... extensions)` / `save(String... extensions)` returning `FileChooserParams`
- `FileChooserParams`: `open`, `allowMultiple`, `title`, `fileName`, `extensions`, `handler`, `multipleHandler`
- `submit`, `submitMulti`, `name`, `extensions`, `open`, `title`, `handleChooseResult`

### `desktop.DesktopLauncher`

Changed platform file chooser methods to use `FileChooserParams` rather than older `showFileChooser(boolean open, String title, String extension, Cons<Fi>)` style.

Migration:
- Client mods calling platform file chooser internals should use `FileChooser.open/save` instead.

### `Menus.label(...)`

New overloads include flags:
- `label(@Nullable String message, int id, float duration, float worldx, float worldy, int flags)`
- `labelReliable(..., flags)`

Migration:
- World label rendering has richer flags; old calls likely still generated through `Call`, but reflection/manual calls need checking.

## 13. Example Mod Scan Findings

Local grep under `源码和示例/mods` found these migration-relevant patterns:

| Mod | Pattern | Risk | Migration |
|---|---|---|---|
| `VoidShield` | `FrameBuffer`, custom shader, `Draw.flush()`, GL framebuffer reflection | High | Add explicit dispose/recreate on reload; reduce flushes; retest with 159 cache pipeline. |
| `VoidShield` | arrays/maps keyed by `exec.build.id` / zone ids | Medium | Building IDs are not content IDs, but still test save/reload and world reload. |
| `MindustryToolMod` | `new Texture(pix)` from network/map images | High | Dispose replaced textures; clear cache on reload/unload. |
| `MindustryToolMod` | `JsonIO.read(String[].class, ...)` normal overload | Low | Normal `read(Class,String)` still exists; only base-object overload changed. |
| `Factoriodustry` | `FrameBuffer`, `Shader`, `CacheLayer` mutation | High | Retest shader layers and dispose/reinit. |
| `BetaMindy` | `CacheLayer.add`, generated `Texture`, arrays by `Team.id` and content IDs | Medium/High | Team IDs are stable; content ID arrays need review where used for blocks/items. |
| `MultiCrafterLib` | `Core.atlas.find`, draw helpers | Medium | Atlas regions may be regenerated by data images; avoid stale dynamic assumptions. |

## 14. Concrete Grep Checklist

Run against a JVM mod:

```bash
grep -RIn --include='*.java' --include='*.kt' -E 'Content\.id|\.id\]|getBy\([^)]*\)\.get\(|JsonIO\.read\([^,]+,[^,]+,[^)]+\)|showSpawns|DirectionalForceProjector|DroneCenter|FrameBuffer|new Shader|new Texture|Pixmap|Draw\.flush|CacheLayer|SaveVersion\.write|writeContentPatches|readContentPatches|state\.patcher' src
```

Fix rules:
- `showSpawns` → `hideSpawns` with inverted meaning.
- `DirectionalForceProjector` / `DroneCenter` → vendor or redesign.
- `JsonIO.read(type, base, json)` → `JsonIO.read(base, json)`.
- `state.patcher` usage → likely `state.data` / `DataManager` route.
- `Content.id` as persistent identity → `(ContentType, name)`.
- GL object creation → explicit disposal + reload reinit.

## 15. Migration Order for Large JVM Mods

1. **Compile layer**: JDK 17, dependency update, remove deleted imports/classes.
2. **Serialization layer**: update `JsonIO`, `SaveOptions`, data patch read/write assumptions.
3. **Content identity layer**: replace ID snapshots, add cache invalidation/rebuild.
4. **Rendering layer**: split static/dynamic draw, add recache hooks, dispose GL resources.
5. **Networking layer**: retest server join and world transfer with assets.
6. **Gameplay/UI layer**: update `hideSpawns`, unit factory delay/status, music rules.
7. **Regression tests**: launch fresh, hot reload/data patch, join server with assets, load save with assets, zoomed-out render, unload/reload mod if supported.

## Related

- [Mindustry 158.1 → 159.1 Mod Migration Guide](mindustry-158-to-1591-mod-migration.md)
- [Mindustry Content Loading Chain](../concepts/mindustry-content-loading-chain.md)
- [Mindustry Rendering System](../mindustry/rendering-system.md)
- [Mindustry Save and Load System](../mindustry/save-load-system.md)

## Appendix: Changed Public/Protected/Private Signature Lines Detected by Diff

This appendix is mechanical. It is included so migration work can search by exact file/symbol. It intentionally includes private internals when the diff exposes lifecycle assumptions useful to reflection-heavy mods.

### `core/src/mindustry/Vars.java`

```diff

-    public static float maxDeltaClient = 6f, maxDeltaServer = 10f;

+    public static float maxDeltaClient = 4f, maxDeltaServer = 4f;

-    public static boolean drawDebugHitboxes = false;

+    public static Fi assetCacheDirectory;

-    public static ExecutorService mainExecutor = Threads.executor("Main Executor", OS.cores);

+    public static ExecutorService mainExecutor = Core.executor;

+    public static DataAssetCache assetCache;

```

### `core/src/mindustry/ai/BlockIndexer.java`

```diff

+    public Building findEnemyTile(Team team, float x, float y, float range, BuildingPriorityf priority, Boolf<Building> pred, @Nullable Team source){

+    public Building findTile(Team team, float x, float y, float range, Boolf<Building> pred, boolean usePriority, @Nullable Team source){

```

### `core/src/mindustry/audio/AmbientSource.java`

```diff

+public interface AmbientSource extends Position{

```

### `core/src/mindustry/audio/MusicContainer.java`

```diff

+public class MusicContainer{

+    public String name = "";

+    private @Nullable Music cached;

+    private boolean accessed;

+    public MusicContainer(String name){

+    public MusicContainer(){

+    public Music get(){

```

### `core/src/mindustry/audio/SoundControl.java`

```diff

-    protected boolean silenced;

+    protected boolean silenced, keepSilent;

+    protected @Nullable AudioThread ambientThread;

+    protected boolean launchingAmbientThread;

+    protected Seq<SoundData> localData = new Seq<>();

-    public void loop(Sound sound, float volume){

-    public void loop(Sound sound, Position pos, float volume){

-    public void loop(Sound sound, Position pos, float volume, float pitch){

-    protected void updateLoops(){

+    public void keepSilent(){

+    public void playMusic(@Nullable Music music, boolean interrupt){

+    public boolean isPlaying(){

+    protected boolean alwaysPlayMusic(){

+    protected Seq<Music> getBossMusic(){

+    protected Seq<Music> getAmbientMusic(){

+    protected Seq<Music> getDarkMusic(){

-    protected static class SoundData{

+    public static @Nullable Music findMusic(String name){

+    public void loop(Sound sound, float volume){

+    public void loop(Sound sound, Position pos, float volume){

+    public void loop(Sound sound, Position pos, float volume, float pitch){

+    public void addAmbientSource(AmbientSource source){

+    protected void updateLoops(){

+    protected static class SoundData implements Cloneable{

+        public SoundData clone(){

+        public void run(){

```

### `core/src/mindustry/content/TechTree.java`

```diff

-        public @Nullable TechNode parent;

+        public @Nullable TechNode parent, rootNode;

```

### `core/src/mindustry/core/ContentLoader.java`

```diff

+    public void remove(@Nullable Content content){

```

### `core/src/mindustry/core/GameState.java`

```diff

-    public DataPatcher patcher = new DataPatcher();

+    public DataManager data = new DataManager();

```

### `core/src/mindustry/core/Logic.java`

```diff

+    protected void updateEntities(){

```

### `core/src/mindustry/core/NetClient.java`

```diff

+    public static void playMusic(String musicName, boolean interrupt){

```

### `core/src/mindustry/core/NetServer.java`

```diff

+    private Seq<Player> playersToSend = new Seq<>(false);

+    private Seq<NetConnection> tempConnections = new Seq<>(false);

+    public long snapshotSyncTime;

+    public void sendWorldAndAssets(Player player){

+    public void sendAssetRequirements(Player player){

-    public static void debugStatusClient(int value, int lastClientSnapshot, int snapshotsSent){

+    public static void debugStatusClient(int value, int lastClientSnapshot){

-    public static void debugStatusClientUnreliable(int value, int lastClientSnapshot, int snapshotsSent){

+    public static void debugStatusClientUnreliable(int value, int lastClientSnapshot){

+    public static void requestWorld(Player player){

+    public static void requestAssets(Player player, short[] ids){

-    public void writeEntitySnapshot(Player player) throws IOException{

+    public void writeStateSnapshot() throws IOException{

+    public void writeEntitySnapshotsAll() throws IOException{

+    public void writeEntitySnapshotsTeam(Team team, Seq<Player> players) throws IOException{

+    protected void sendEntitySnapshots(Seq<NetConnection> connections, short amount, byte[] data){

+    public void writeCustomEntitySnapshot(Player player, Iterable<Syncc> entities) throws IOException{

+    protected void writeEntity(Syncc entity, DataOutputStream dataStream) throws IOException{

```

### `core/src/mindustry/core/PerfCounter.java`

```diff

+    private long lastUpdateFrame = -1;

+    private long partValue;

+    public void add(long nanos){

+    public void end(long subtract){

+    public void checkUpdate(){

+    public void beginPart(){

+    public void endPart(){

+    public void finishParts(){

+    public long latestValueNs(){

```

### `core/src/mindustry/core/UI.java`

```diff

+    public PerformanceFragment perffrag;

-    private final IntMap<Table> labels = new IntMap<>();

+    private final IntMap<WorldLabel> labels = new IntMap<>();

-    public void showLabel(@Nullable String info, int id, float duration, float worldx, float worldy){

+    public void showLabel(@Nullable String info, int id, float duration, float worldx, float worldy, int flags){

```

### `core/src/mindustry/ctype/Content.java`

```diff

+    public boolean removed;

+    public void removeContent(){}

+    public boolean isPatchContent(){

+        public @Nullable ContentAsset asset;

```

### `core/src/mindustry/ctype/ContentType.java`

```diff

+    public final String folderName;

```

### `core/src/mindustry/editor/MapEditorDialog.java`

```diff

+    public void rebuildBlockSelection(){

```

### `core/src/mindustry/editor/MapInfoDialog.java`

```diff

-    private MapPatchesDialog patches = new MapPatchesDialog();

+    private MapAssetsDialog patches = new MapAssetsDialog();

```

### `core/src/mindustry/editor/MapPatchesDialog.java`

```diff

-public class MapPatchesDialog extends BaseDialog{

-    private Table list;

-    public MapPatchesDialog(){

-    private void setup(){

```

### `core/src/mindustry/editor/data/AssetView.java`

```diff

+public interface AssetView{

```

### `core/src/mindustry/editor/data/MapAssetsDialog.java`

```diff

+public class MapAssetsDialog extends BaseDialog{

+    private Table list;

+    private DataAssetType currentType = DataAssetType.music;

+    public MapAssetsDialog(){

```

### `core/src/mindustry/editor/data/MapAudioView.java`

```diff

+public class MapAudioView implements AssetView{

+    public final DataAssetType type;

+    public @Nullable AudioSource lastPlaying;

+    public MapAudioView(DataAssetType type){

+    public void build(MapAssetsDialog diag, Table list){

+            public boolean remove(){

+            public void act(float delta){

+    public void buildButtons(MapAssetsDialog diag, Table buttons){

```

### `core/src/mindustry/editor/data/MapBundlesView.java`

```diff

+public class MapBundlesView implements AssetView{

+    public void build(MapAssetsDialog diag, Table list){

+    public void buildButtons(MapAssetsDialog diag, Table buttons){

```

### `core/src/mindustry/editor/data/MapContentView.java`

```diff

+public class MapContentView implements AssetView{

+    private static final Pattern unsafeNamePattern = Pattern.compile("[\\[\\]{}`!@#$%^&8*();:,]");

+    public void build(MapAssetsDialog diag, Table list){

+    public void buildButtons(MapAssetsDialog diag, Table buttons){

```

### `core/src/mindustry/editor/data/MapImagesView.java`

```diff

+public class MapImagesView implements AssetView{

+    public void buildButtons(MapAssetsDialog diag, Table buttons){

+    public void build(MapAssetsDialog diag, Table inner){

+    private Seq<ImageAsset> getImages(){

```

### `core/src/mindustry/editor/data/MapPatchesView.java`

```diff

+public class MapPatchesView implements AssetView{

+    public void buildButtons(MapAssetsDialog diag, Table buttons){

+    public void build(MapAssetsDialog diag, Table list){

```

### `core/src/mindustry/entities/EntityGroup.java`

```diff

+    private double fixedCounter, timeCounter;

+    private long lastTimeAccess = -1;

+    private Seq<DelayRun> timeRuns = new Seq<>();

+    public void fixedUpdate(int targetFps){

+    public void fixedUpdate(int targetUps, int maxUpdatesPerFrame){

```

### `core/src/mindustry/entities/Units.java`

```diff

+    public static Teamc closestTarget(Team team, float x, float y, float range, @Nullable Team sourceTeam, Boolf<Unit> unitPred, Boolf<Building> tilePred){

```

### `core/src/mindustry/entities/abilities/EmptyDataAbility.java`

```diff

+public class EmptyDataAbility extends Ability{

```

### `core/src/mindustry/entities/comp/BuildingComp.java`

```diff

+    public boolean inMapArea(){

-    public void payloadDraw(){

+    public void drawCached(){

+    public void recache(){

+    public void payloadDraw(){

+    public float getAmbientVolume(){

+    public Sound getAmbientSound(){

```

### `core/src/mindustry/entities/comp/UnitComp.java`

```diff

-    public boolean isSyncHidden(Player player){

+    public boolean isSyncHidden(Team team){

```

### `core/src/mindustry/entities/comp/WorldLabelComp.java`

```diff

+    public transient float duration = -1;

+    public transient @Nullable Runnable expired;

+    public void update(){

+    public void show(Player player){

+    public void hide(Player player){

```

### `core/src/mindustry/entities/effect/NoiseEffect.java`

```diff

+public class NoiseEffect extends Effect{

+    public String noisePath = "sprites/distortAlpha.png";

+    public @Nullable Color color;

+    public float noiseScl = 1000f;

+    public float opacity = 0.3f;

+    public float baseSpeed = 0.4f;

+    public float intensity = 1f;

+    public float windX = 1f;

+    public float windY = 0f;

+    public int layers = 4;

+    public float layerSpeedMul = -1.3f;

+    public float layerAlphaMul = 0.7f;

+    public float layerSclMul = 0.8f;

+    public float layerColorMul = 0.9f;

+    public Texture tex;

+    public void render(EffectContainer e){

+    public static void drawNoise(Texture noise, Color color, float noisescl, float opacity, float baseSpeed, float intensity, float vwindx, float vwindy, float offset){

```

### `core/src/mindustry/entities/part/DrawPart.java`

```diff

+    private static class CompatFix{

```

### `core/src/mindustry/game/CampaignRules.java`

```diff

-    public boolean showSpawns;

+    public boolean hideSpawns;

```

### `core/src/mindustry/game/EventType.java`

```diff

-    public static class ContentPatchLoadEvent{

-        public final Seq<String> patches;

+    public static class DataPatchLoadEvent{

+        public final Seq<DataAsset> assets;

-        public ContentPatchLoadEvent(Seq<String> patches){

+        public DataPatchLoadEvent(Seq<DataAsset> assets){

```

### `core/src/mindustry/game/Rules.java`

```diff

-    public boolean showSpawns = false;

+    public boolean hideSpawns = true;

+    public float unitFactoryActivationDelay = 0f;

+    public @Nullable Seq<MusicContainer> ambientMusic;

+    public @Nullable Seq<MusicContainer> darkMusic;

+    public boolean alwaysPlayMusic = false;

+    public void retainContentFields(Rules source){

+    public float unitActivationDelay(Team team){

```

### `core/src/mindustry/game/Saves.java`

```diff

+        public boolean isBeingPlayed(){

+        public boolean hasExternalAssets(){

```

### `core/src/mindustry/graphics/BlockRenderer.java`

```diff

-    public static final int crackRegions = 8, maxCrackSize = 7;

-    public static boolean drawQuadtreeDebug = false;

+    public static final int crackRegions = 8, maxCrackSize = 7, chunkSize = 30, maxSpritesPerCacheTile = 3;

+    public static final boolean drawQuadtreeDebug = false;

+    public static final boolean blockDrawCountDebug = false;

+    private IntSeq chunksToDraw = new IntSeq();

+    private IntSet chunksToDrawSet = new IntSet();

+    private Seq<Building> tileExtraCachedView = new Seq<>(false, initialRequests, Building.class);

+    private Seq<Building> tileWithConsumerView = new Seq<>(false, initialRequests, Building.class);

+    private Team lastTeam;

+    private BlockQuadtree blockCachedTree = new BlockQuadtree(new Rect(0, 0, 1, 1));

+    private CacheChunk[][] cacheChunks;

+    private CacheBatch cbatch = new CacheBatch(null);

+    private Seq<SpriteCache>[] caches = new Seq[BuildingCacheLayer.amount];

+    private Seq<IntSeq>[] queuedCacheDraws = new Seq[BuildingCacheLayer.amount];

+    private IntSeq[] queuedCacheIndices = new IntSeq[BuildingCacheLayer.amount];

+    private IntSet dirtyChunks = new IntSet();

+    private ObjectIntMap<Block> blockDrawSprites = new ObjectIntMap<>();

+    private ObjectIntMap<Block> blockDrawTotal = new ObjectIntMap<>();

+    public void recacheBuilding(BuildingCacheLayer layer, Tile tile){

+    public void cacheChunk(int layer, int cx, int cy){

+                public Entry(Block block, int amount){

+                public String toString(){

+                public int compareTo(Entry entry){

```

### `core/src/mindustry/graphics/BuildingCacheLayer.java`

```diff

+public enum BuildingCacheLayer{

+    public final float layer;

+    public final static BuildingCacheLayer[] all = values();

+    public final static int amount = all.length;

+    public final static float[] layers = new float[all.length];

```

### `core/src/mindustry/graphics/Drawf.java`

```diff

+    public static void laser(TextureRegion line, TextureRegion start, TextureRegion end, float x, float y, float x2, float y2, float scale, boolean light, boolean useLod){

```

### `core/src/mindustry/graphics/FloorRenderer.java`

```diff

-    private IndexData indexData;

-            public void dispose(){

-    public IndexData getIndexData(){

```

### `core/src/mindustry/graphics/Lod.java`

```diff

+public class Lod{

+    private static final float threshold1 = 1.4f, threshold2 = 0.8f;

+    private static final float fade = 0.2f;

+    public static boolean disable = false;

+    public static boolean l1 = true;

+    public static float alpha1 = 1f;

+    public static boolean l2 = true;

+    public static float alpha2 = 1f;

+    public static void update(){

```

### `core/src/mindustry/graphics/MultiPacker.java`

```diff

-    private PixmapPacker[] packers = new PixmapPacker[PageType.all.length];

+    private PixmapPacker[] packers;

+    public MultiPacker(boolean initialize){

-    public PixmapRegion get(String name){

+    public PixmapRegion get(TextureRegion region){

+    public PixmapRegion get(String region){

+    public @Nullable PixmapRegion getPacked(String name){

-    public PixmapPacker getPacker(PageType type){

```

### `core/src/mindustry/io/JsonIO.java`

```diff

-    private static final CustomJson jsonBase = new CustomJson();

-    public static <T> T read(Class<T> type, T base, String string){

+    public static <T> T read(T base, String string){

+            public void write(Json json, MusicContainer object, Class knownType){

+            public MusicContainer read(Json json, JsonValue jsonData, Class type){

-        private Object baseObject;

-        public <T> T fromJson(Class<T> type, String json){

-        public <T> T fromBaseJson(Class<T> type, T base, String json){

-        protected Object newInstance(Class type){

```

### `core/src/mindustry/io/MapIO.java`

```diff

+    public static void writeMap(Fi file, Map map, boolean embed) throws IOException{

```

### `core/src/mindustry/io/SaveIO.java`

```diff

-    public static final Seq<SaveVersion> versionArray = Seq.with(new Save1(), new Save2(), new Save3(), new Save4(), new Save5(), new Save6(), new Save7(), new Save8(), new Save9(), new Save10(), new Save11());

+    public static final Seq<SaveVersion> versionArray = Seq.with(new Save1(), new Save2(), new Save3(), new Save4(), new Save5(), new Save6(), new Save7(), new Save8(), new Save9(), new Save10(), new Save11(), new Save1

+    public static void save(Fi file, SaveOptions options){

-    public static void write(Fi file, StringMap tags){

+    public static void write(Fi file, SaveOptions options){

-    public static void write(OutputStream os, StringMap tags){

+    public static void write(OutputStream os, SaveOptions options){

```

### `core/src/mindustry/io/SaveOptions.java`

```diff

+public class SaveOptions{

+    public boolean embedAssets;

+    public @Nullable StringMap extraTags;

```

### `core/src/mindustry/io/SaveReadState.java`

```diff

+public class SaveReadState{

+    public final WorldContext context;

+    public @Nullable String ruleString;

+    public SaveReadState(WorldContext context){

```

### `core/src/mindustry/io/SaveVersion.java`

```diff

-    public final void write(DataOutputStream stream) throws IOException{

-    public void read(DataInputStream stream, CounterInputStream counter, WorldContext context) throws IOException{

+    public void read(DataInputStream stream, CounterInputStream counter, SaveReadState saveState) throws IOException{

-    public void write(DataOutputStream stream, StringMap extraTags) throws IOException{

+    public void write(DataOutputStream stream, SaveOptions options) throws IOException{

-    public void readMeta(DataInput stream, WorldContext context) throws IOException{

+    public void readMeta(DataInput stream, SaveReadState saveState) throws IOException{

+    public void readRules(SaveReadState saveState){

+    private void writeEntity(Entityc entity, DataOutput stream) throws IOException{

-    public void skipContentPatches(DataInput stream) throws IOException{

+    public void readDataPatches(DataInput stream, SaveReadState saveState) throws IOException{

-    public void readContentPatches(DataInput stream) throws IOException{

-    public void writeContentPatches(DataOutput stream) throws IOException{

+    public void writeDataPatches(DataOutput stream, boolean forceEmbed) throws IOException{

```

### `core/src/mindustry/io/TypeIO.java`

```diff

+    public static void writeShorts(Writes write, short[] ints){

+    public static short[] readShorts(Reads read){

```

### `core/src/mindustry/io/versions/LegacyRegionSaveVersion.java`

```diff

-    public void read(DataInputStream stream, CounterInputStream counter, WorldContext context) throws IOException{

+    public void read(DataInputStream stream, CounterInputStream counter, SaveReadState saveState) throws IOException{

```

### `core/src/mindustry/io/versions/Save11.java`

```diff

+    public void read(DataInputStream stream, CounterInputStream counter, SaveReadState saveState) throws IOException{

+    public void readDataPatches(DataInput stream, SaveReadState saveState) throws IOException{

```

### `core/src/mindustry/io/versions/Save12.java`

```diff

+public class Save12 extends SaveVersion{

+    public Save12(){

+    public void readDataPatches(DataInput stream, SaveReadState saveState) throws IOException{

```

### `core/src/mindustry/io/versions/Save13.java`

```diff

+public class Save13 extends SaveVersion{

+    public Save13(){

```

### `core/src/mindustry/logic/GlobalVars.java`

```diff

-    private static LVar varTime, varTick, varSecond, varMinute, varWave, varWaveTime, varMapW, varMapH, varWait, varServer, varClient, varClientLocale, varClientUnit, varClientName, varClientTeam, varClientMobile;

-    public static final Seq<String> soundNames = new Seq<>();

-    public LVar get(String name){

+    public @Nullable LVar get(String name){

-    public LVar get(String name, boolean privileged){

+    public @Nullable LVar get(String name, boolean privileged){

+    public void remove(LVar lvar){

+    public void remove(String name){

```

### `core/src/mindustry/logic/LExecutor.java`

```diff

-    public boolean yield;

+    public boolean yield, stop;

+    public static class PlayMusicI implements LInstruction{

+        public LVar name, interrupt;

+        public PlayMusicI(){

+        public PlayMusicI(LVar name, LVar interrupt){

+        public void run(LExecutor exec){

```

### `core/src/mindustry/logic/LStatements.java`

```diff

+    public static class PlayMusicStatement extends LStatement{

+        public String name = "\"game1\"", interrupt = "true";

+        public void build(Table table){

+        public boolean privileged(){

+        public LInstruction build(LAssembler builder){

+        public LCategory category(){

```

### `core/src/mindustry/logic/LogicScript.java`

```diff

+public class LogicScript implements JsonSerializable{

+    public static final int defaultTimeoutMs = 200, maxTimeoutMs = 2000;

+    public LogicScript(String script){

+    public void run(){

+    public void write(Json json){

+    public void read(Json json, JsonValue jsonData){

```

### `core/src/mindustry/maps/Maps.java`

```diff

+    public Map saveMap(ObjectMap<String, String> baseTags){

-    public Map saveMap(ObjectMap<String, String> baseTags){

+    public Map saveMap(ObjectMap<String, String> baseTags, boolean embedAssets){

```

### `core/src/mindustry/maps/generators/FileMapGenerator.java`

```diff

-    public final Map map;

+    public final @Nullable Map map;

-    public FileMapGenerator(Map map, SectorPreset preset){

+    public FileMapGenerator(@Nullable Map map, SectorPreset preset){

```

### `core/src/mindustry/mod/DataAssetCache.java`

```diff

+public class DataAssetCache{

+    private static final char[] base32Alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".toCharArray();

+    private ObjectMap<String, Fi> hashToFile = new ObjectMap<>();

+    public void load(){

+    public synchronized void addOverride(String hash, Fi source){

+    public synchronized byte[] add(byte[] bytes){

+    public boolean has(String hash){

+    public synchronized @Nullable Fi get(String shaHash){

+    public @Nullable Fi get(byte[] shaHash){

+    public static String encodeHash(byte[] data){

```

### `core/src/mindustry/mod/DataAudioLoader.java`

```diff

+public class DataAudioLoader{

+    private static final int soundIdOffset = 100_000;

+    public static final String prefix = "dp-";

+    private Seq<Sound> loadedSounds = new Seq<>();

+    private Seq<Music> loadedMusic = new Seq<>();

+    private ObjectSet<String> registered = new ObjectSet<>();

+    public void load(Seq<SoundAsset> sounds, Seq<MusicAsset> musics){

+    public void unload(){

```

### `core/src/mindustry/mod/DataBundleLoader.java`

```diff

+public class DataBundleLoader{

+    private ObjectMap<I18NBundle, ObjectMap<String, String>> originalProperties = new ObjectMap<>();

+    public void load(Seq<BundleAsset> assets){

+    public void unload(){

```

### `core/src/mindustry/mod/DataImagePacker.java`

```diff

+public class DataImagePacker{

+    public static final String regionPrefix = "dp-";

+    private @Nullable TextureAtlas patchAtlas;

+    public void pack(Seq<ImageAsset> images){

+    public void unload(){

+    public void printStats(PixmapPacker mainPacker, PixmapPacker envPacker){

+        public PackResult(String name, Pixmap pixmap){

```

### `core/src/mindustry/mod/DataManager.java`

```diff

+public class DataManager{

+    private DataPatcher patcher = new DataPatcher();

+    private DataImagePacker packer = new DataImagePacker();

+    private DataAudioLoader soundLoader = new DataAudioLoader();

+    private DataBundleLoader bundleLoader = new DataBundleLoader();

+    private ObjectMap<DataAssetType, Seq<DataAsset>> assets = new ObjectMap<>();

+    private Seq<DataAsset> orderedAssets = new Seq<>();

+    private Seq<DataAsset> orderedExternalAssets = new Seq<>();

+    public void reloadContent(boolean reloadArrays){

+    public void regenerateContentSprites(boolean forcePack){

+            public void add(PageType type, String name, PixmapRegion region, int[] splits, int[] pads){

+            public boolean has(String name){

+            public boolean has(PageType type, String name){

+            public @Nullable PixmapRegion get(TextureRegion region){

+            public PixmapRegion get(String name){

+    public void reloadPatches(Seq<PatchAsset> patches){

+    public void reloadImages(){

+    public void reloadImages(Seq<ImageAsset> images){

+    public void reloadAudio(){

+    public void load(Seq<DataAsset> newAssets){

+    public void unload(){

+    private void rebuildOrderedAssets(){

+    public Seq<DataAsset> getMissingAssets(){

+    public Seq<DataAsset> getAllAssets(){

+    public boolean hasAssetPath(DataAssetType type, String path){

+    public boolean hasAssetName(DataAssetType type, String name){

+    public boolean hasAssetNameOrPath(DataAssetType type, String name, String path){

+    public boolean hasExternalAssets(){

+    public Seq<DataAsset> getAllExternalAssets(){

+    public boolean isPatched(Object content){

+    public <T extends DataAsset> Seq<T> getAssets(DataAssetType type){

+    public void clearGeneratedImages(){

+    public Seq<ImageAsset> getImages(){

+    public Seq<PatchAsset> getPatches(){

+    public Seq<MusicAsset> getMusic(){

+    public Seq<SoundAsset> getSounds(){

+    public Seq<BundleAsset> getBundles(){

+    public Seq<ContentAsset> getContent(){

```

### `core/src/mindustry/mod/DataPatcher.java`

```diff

+    public static final LoadedMod dpMod = new LoadedMod(new Fi("dp"), new Fi(""), null, null, dpModMeta);

+    public static final int maxImageSize = 2000;

+    public static final int patchFormatVersion = 2;

+    private static boolean needsArrayFix = false;

+    private static DataPatcher currentDataPatcher;

-    private @Nullable PatchSet currentlyApplying;

-    public Seq<PatchSet> patches = new Seq<>();

+    private Seq<Content> addedContent = new Seq<>();

+    private @Nullable PatchAsset currentlyApplyingPatch;

+    private @Nullable ContentAsset currentlyApplyingContent;

+    private Seq<LVar> addedVars = new Seq<>();

-    public void apply(Seq<String> patchArray) throws Exception{

+    public void apply(Seq<PatchAsset> patches, Seq<ContentAsset> content){

+    public void apply(Seq<PatchAsset> patches, Seq<ContentAsset> content, boolean reloadContentWorld){

+    public void unapply(boolean reloadContentWorld){

+    public Seq<Content> getAddedContent(){

+    public static void fixContentArrays(){

-    public static class PatchSet{

-        public String patch;

-        public JsonValue json;

-        public String name = "";

-        public boolean error;

-        public Seq<String> warnings = new Seq<>();

-        public PatchSet(String patch, JsonValue json){

-        public String toString(){

```

### `core/src/mindustry/mod/Mods.java`

```diff

-    private static final ObjectSet<String> blacklistedMods = ObjectSet.with("ui-lib", "braindustry", "schema", "scheme-size:1.0.5", "scheme-size:1.0.4", "scheme-size:1.0.3", "scheme-size:1.0.1", "scheme-size:1.0.0", "sc

```

### `core/src/mindustry/mod/Scripts.java`

```diff

+    public Class<?> getClass(Object object){

```

### `core/src/mindustry/mod/data/BundleAsset.java`

```diff

+public class BundleAsset extends DataAsset{

+    public @Nullable OrderedMap<String, String> cachedBundle;

+    public void tryLoadCache(){

+    public void updateData(byte[] data){

+    public DataAssetType getType(){

```

### `core/src/mindustry/mod/data/ContentAsset.java`

```diff

+public class ContentAsset extends DataAsset{

+    public static final ContentType[] loadableContent = {ContentType.item, ContentType.block, ContentType.liquid, ContentType.status, ContentType.unit, ContentType.weather};

+    public ContentType type = ContentType.unit;

+    public String data = "";

+    public Seq<String> warnings = new Seq<>();

+    public @Nullable Content content;

+    public boolean errored;

+    public ContentAsset(String path, ContentType type, String data){

+    public ContentAsset(){

+    public String hashData(){

+    public void readOverride(String path, Fi file, ContentType type) throws IOException{

+    public String getFullPath(){

+    public byte[] getData(){

+    public DataAssetType getType(){

+    public void read(DataInput stream) throws IOException{

+    public void write(DataOutput stream) throws IOException{

+    public int compareTo(DataAsset asset){

```

### `core/src/mindustry/mod/data/DataAsset.java`

```diff

+public abstract class DataAsset implements Comparable<DataAsset>{

+    private @Nullable Fi overrideCacheFile;

+    public String path = "";

+    public String name = "";

+    public @Nullable byte[] byteHash;

+    public @Nullable String stringHash;

+    public void updateData(byte[] data){

+    public void setHash(byte[] value){

+    public void setPath(String path){

+    public String getFullPath(){

+    public byte[] getData(){

+    public boolean isAlwaysEmbedded(){

+    public boolean isCached(){

+    public Fi getCacheFileNoNull(){

+    public @Nullable Fi getCacheFile(){

+    public void readFromZip(String path, Fi file){

+    public void readOverride(String path, Fi file) throws IOException{

+    public void read(DataInput stream) throws IOException{

+    public void write(DataOutput stream) throws IOException{

+    public String toString(){

+    public int compareTo(DataAsset asset){

```

### `core/src/mindustry/mod/data/DataAssetType.java`

```diff

+public enum DataAssetType{

+    public final String folder;

+    public final Seq<String> extensions;

+    public final Prov<DataAsset> constructor;

+    public final boolean embedded;

+    public static final DataAssetType[] all = values();

+    public DataAsset create(){

+    public String localized(){

```

### `core/src/mindustry/mod/data/ImageAsset.java`

```diff

+public class ImageAsset extends DataAsset{

+    public ImageAsset(){}

+    public ImageAsset(String path, byte[] hash){

+    public boolean isGenerated(){

+    public DataAssetType getType(){

```

### `core/src/mindustry/mod/data/MusicAsset.java`

```diff

+public class MusicAsset extends DataAsset{

+    public DataAssetType getType(){

```

### `core/src/mindustry/mod/data/PatchAsset.java`

```diff

+public class PatchAsset extends DataAsset{

+    private static final JsonValue emptyValue = new JsonValue("error");

+    public String patch = "";

+    public JsonValue json = emptyValue;

+    public boolean error;

+    public Seq<String> warnings = new Seq<>();

+    public PatchAsset(String patch){

+    public byte[] getData(){

+    public void readFromZip(String path, Fi file){

+    public void readOverride(String path, Fi file) throws IOException{

+    public DataAssetType getType(){

+    public void read(DataInput stream) throws IOException{

+    public void write(DataOutput stream) throws IOException{

+    public String toString(){

```

### `core/src/mindustry/mod/data/SoundAsset.java`

```diff

+public class SoundAsset extends DataAsset{

+    public DataAssetType getType(){

```

### `core/src/mindustry/net/Administration.java`

```diff

+    public ObjectMap<String, PlayerInfo> playerInfo = new ObjectMap<>();

-    public ObjectMap<String, PlayerInfo> playerInfo = new ObjectMap<>();

+    private ObjectMap<String, IdEncounterInfo> encounteredIDsForIp = new ObjectMap<>();

+    public boolean checkUuidChanges(String address, String uuid){

```

### `core/src/mindustry/net/ArcNetProvider.java`

```diff

+    private static final ThreadLocal<Seq<Connection>> writeConnections = Threads.local(Seq::new);

+    public void sendAllServer(Object object, Iterable<NetConnection> connections, boolean reliable){

+    public void sendAllServer(Object object, boolean reliable){

+    public void sendExceptServer(NetConnection except, Object object, boolean reliable){

+    public class ArcConnection extends NetConnection{

+        public void blacklist(){

```

### `core/src/mindustry/net/Net.java`

```diff

+    public static final int packetIdAssetStream, packetIdWorldStream;

-    public static <T extends Packet> void registerPacket(Prov<T> cons){

+    public static <T extends Packet> int registerPacket(Prov<T> cons){

+    public static byte getPacketClassId(Class<?> c){

+    public void send(Object object, Iterable<NetConnection> connections, boolean reliable){

```

### `core/src/mindustry/net/NetConnection.java`

```diff

-    public long syncTime;

-    public int snapshotsSent;

+    public Seq<Syncc> localEntities = new Seq<>(false);

-    public boolean hasConnected, hasBegunConnecting, hasDisconnected;

+    public boolean hasConnected, hasBegunConnecting, determiningAssets, receivingAssets, hasDisconnected;

+    public void sendStreamAsync(Streamable stream, ByteArrayOutputStream data){

+    public void sendStream(Streamable stream, ByteArrayOutputStream data){

+    public void blacklist(){

```

### `core/src/mindustry/net/NetworkIO.java`

```diff

+    public static void writeRequiredAssets(OutputStream os, Seq<DataAsset> assets){

+    public static Seq<String> readRequiredAssets(InputStream is){

+    public static void writeAssets(OutputStream os, Seq<DataAsset> assets){

+    public static void loadAssets(InputStream is) throws IOException{

```

### `core/src/mindustry/net/Packets.java`

```diff

+    public static class AssetRequirementStream extends Streamable{

+    public static class AssetStream extends Streamable{

+        public boolean incremental(){

+        public @Nullable transient InputStream incrementalStream;

```

### `core/src/mindustry/net/Streamable.java`

```diff

+    public boolean incremental(){

+        public final boolean incremental;

+        public final IncrementalStream incrementalStream;

+        public int received;

-        public StreamBuilder(StreamBegin begin){

+        public StreamBuilder(StreamBegin begin, boolean incremental){

+        public void close(){

+        private static final byte[] closedSignal = new byte[0];

+        private final LinkedBlockingQueue<byte[]> queue = new LinkedBlockingQueue<>();

+        private volatile boolean isClosed, isFinished;

+        private byte[] currentBuffer = null, singleByte = new byte[1];

+        private int currentOffset = 0;

+        public int read(byte[] b, int off, int len) throws IOException{

+        public int read() throws IOException{

+        public void finish(){

+        public void close(){

```

### `core/src/mindustry/type/Planet.java`

```diff

+    public @Nullable Seq<Music> ambientMusic;

+    public @Nullable Seq<Music> darkMusic;

+    public boolean alwaysPlayMusic = false;

-    public Planet(String name, Planet parent, float radius){

+    public Planet(String name, @Nullable Planet parent, float radius){

+    public void removeContent(){

```

### `core/src/mindustry/type/SectorDifficulty.java`

```diff

+public class SectorDifficulty{

```

### `core/src/mindustry/type/SectorPreset.java`

```diff

+    public void removeContent(){

```

### `core/src/mindustry/ui/BorderImage.java`

```diff

+    public boolean forceNearest = false, drawAlpha = false;

+    public Color alphaColor = Color.gray.cpy();

```

### `core/src/mindustry/ui/FileChooser.java`

```diff

+public class FileChooser{

+    public static void export(String name, String extension, FileWriter writer){

+    public static FileChooserParams open(String... extensions){

+    public static FileChooserParams save(String... extensions){

+    public static FileChooserDialog createFallbackFileChooser(FileChooserParams params){

+    public static void showFallbackFileChooser(FileChooserParams params){

+    public static class FileChooserParams{

+        public boolean open;

+        public boolean allowMultiple;

+        public String title;

+        public String fileName;

+        public String[] extensions;

+        public @Nullable Cons<Fi> handler;

+        public @Nullable Cons<Fi[]> multipleHandler;

+        private void checkParams(){

+        public void submit(Cons<Fi> handler){

+        public void submitMulti(Cons<Fi[]> multipleHandler){

+        public FileChooserParams name(String fileName){

+        public FileChooserParams extensions(String... extensions){

+        public FileChooserParams open(boolean open){

+        public FileChooserParams title(String title){

+        public void handleChooseResult(Fi... files){

```

### `core/src/mindustry/ui/Menus.java`

```diff

+    public static void label(@Nullable String message, int id, float duration, float worldx, float worldy, int flags){

+    public static void labelReliable(@Nullable String message, int id, float duration, float worldx, float worldy, int flags){

```

### `core/src/mindustry/ui/dialogs/BaseDialog.java`

```diff

+    protected Image titleImage;

+    public void setTitleColor(Color color){

```

### `core/src/mindustry/ui/dialogs/FileChooserDialog.java`

```diff

+public class FileChooserDialog extends BaseDialog{

+    private static final Fi homeDirectory = Core.files.absolute(Core.files.getExternalStoragePath());

+    private Table files;

+    private ScrollPane pane;

+    private TextField navigation, filefield;

+    private TextButton ok;

+    private FileHistory stack = new FileHistory();

+    private Boolf<Fi> filter;

+    private Cons<Fi> selectListener;

+    private boolean open;

+    public FileChooserDialog(String title, Boolf<Fi> filter, boolean open, Cons<Fi> result){

+    private void setupWidgets(){

+    private void updateFileFieldStatus(){

+    private Fi[] getFileNames(){

+    public static synchronized void setLastDirectory(Fi directory){

+    public static synchronized Fi getLastDirectory(){

+    public class FileHistory{

+        private Seq<Fi> history = new Seq<>();

+        private int index;

+        public FileHistory(){

+        public void push(Fi file){

+        public void back(){

+        public void forward(){

+        public boolean canForward(){

+        public boolean canBack(){

```

### `core/src/mindustry/ui/dialogs/MapListDialog.java`

```diff

+    private ObjectMap<Map, Rules> rulesCache = new ObjectMap<>();

```

### `core/src/mindustry/ui/dialogs/PausedDialog.java`

```diff

+    public void runExitSave(boolean save){

```

### `core/src/mindustry/ui/dialogs/SettingsMenuDialog.java`

```diff

+    public SettingsTable dev;

```

### `core/src/mindustry/ui/fragments/HudFragment.java`

```diff

+    private float coreAttackTime;

```

### `core/src/mindustry/ui/fragments/LoadingFragment.java`

```diff

+    public void showProgressBar(){

+    public boolean showingProgress(){

+    public void setText(String text, Color color){

-    private void text(String text){

```

### `core/src/mindustry/ui/fragments/PerformanceFragment.java`

```diff

+public class PerformanceFragment{

+    private static Color color(PerfCounter counter){

+    public void build(Group parent){

+        public void draw(){

```

### `core/src/mindustry/world/Block.java`

```diff

+    public boolean drawDynamic = true;

+    public boolean drawCached = false;

+    public BuildingCacheLayer buildingCacheLayer = BuildingCacheLayer.normal;

+    public void drawBaseCached(Tile tile){

+    public void checkContentArrayCapacity(int items, int liquids){

```

### `core/src/mindustry/world/Tile.java`

```diff

+    public boolean inMapArea(){

+    private static void reflowPower(Team team, Building build){

```

### `core/src/mindustry/world/blocks/campaign/Accelerator.java`

```diff

+    public void checkContentArrayCapacity(int items, int liquids){

```

### `core/src/mindustry/world/blocks/defense/DirectionalForceProjector.java`

```diff

-public class DirectionalForceProjector extends Block{

-    protected static final Vec2 intersectOut = new Vec2(), p1 = new Vec2(), p2 = new Vec2();

-    protected static DirectionalForceProjectorBuild paramEntity;

-    protected static Effect paramEffect;

-    public float width = 30f;

-    public float shieldHealth = 3000f;

-    public float cooldownNormal = 1.75f;

-    public float cooldownLiquid = 1.5f;

-    public float cooldownBrokenBase = 0.35f;

-    public Effect absorbEffect = Fx.absorb;

-    public Effect shieldBreakEffect = Fx.shieldBreak;

-    public float length = 40f;

-    public float padSize = 40f;

-    public DirectionalForceProjector(String name){

-    public void init(){

-    public void setBars(){

-    public boolean outputsItems(){

-    public void setStats(){

-    public void drawPlace(int x, int y, int rotation, boolean valid){

-    public class DirectionalForceProjectorBuild extends Building{

-        public boolean broken = true;

-        public float buildup, hit, warmup, shieldRadius;

-        public boolean shouldAmbientSound(){

-        public void pickedUp(){

-        public void updateTile(){

-        public void deflectBullets(){

-        public void draw(){

-        public void drawShield(){

-        public void write(Writes write){

-        public void read(Reads read, byte revision){

```

### `core/src/mindustry/world/blocks/defense/MendProjector.java`

```diff

-        public void draw(){

+        public void drawCached(){

+        public void draw(){

```

### `core/src/mindustry/world/blocks/defense/ShieldWall.java`

```diff

+    public void init(){

```

### `core/src/mindustry/world/blocks/defense/Thruster.java`

```diff

-        public void draw(){

+        public void drawCached(){

```

### `core/src/mindustry/world/blocks/defense/Wall.java`

```diff

+    public boolean autotile = false;

+    protected TextureRegion[] autotileRegions;

+    public void load(){

-        public float hit;

+        protected int autotileBits;

+        protected float hit;

+        protected void updateAutotileBits(){

+        protected void updateOtherBits(){

+        public void onProximityUpdate(){

+        public void onProximityRemoved(){

+        public void onProximityAdded(){

+        public void drawCached(){

```

### `core/src/mindustry/world/blocks/defense/turrets/Turret.java`

```diff

-        public boolean wasShooting;

+        public boolean wasShooting, isShooting;

+        public boolean controlled(){

```

### `core/src/mindustry/world/blocks/distribution/DirectionBridge.java`

```diff

-        public void draw(){

+        public void drawCached(){

+        public void draw(){

```

### `core/src/mindustry/world/blocks/distribution/DirectionLiquidBridge.java`

```diff

-        public void draw(){

+        public void drawCached(){

+        public void draw(){

```

### `core/src/mindustry/world/blocks/distribution/Duct.java`

```diff

-    public Color transparentColor = new Color(0.4f, 0.4f, 0.4f, 0.1f);

+        public void drawCached(){

+        public void draw(boolean under){

-        protected void drawAt(float x, float y, int bits, float rotation, SliceMode slice){

+        protected void drawAt(float x, float y, int bits, float rotation, SliceMode slice, boolean under){

```

### `core/src/mindustry/world/blocks/distribution/ItemBridge.java`

```diff

-        public boolean wasMoved, moved;

+        public boolean wasMoved, moved, hadValidLink;

```

### `core/src/mindustry/world/blocks/distribution/StackConveyor.java`

```diff

-        public void draw(){

+        public void drawCached(){

+        public void draw(){

```

### `core/src/mindustry/world/blocks/distribution/StackRouter.java`

```diff

-        public void draw(){

+        public void drawCached(){

+        public void draw(){

```

### `core/src/mindustry/world/blocks/environment/StaticProp.java`

```diff

+public class StaticProp extends Prop{

+    public StaticProp(String name){

```

### `core/src/mindustry/world/blocks/heat/HeatConductor.java`

```diff

+        public double sense(LAccess sensor){

```

### `core/src/mindustry/world/blocks/heat/HeatProducer.java`

```diff

+        public double sense(LAccess sensor){

```

### `core/src/mindustry/world/blocks/legacy/LegacyCommandCenter.java`

```diff

-    public class CommandBuild extends Building{

+    public class LegacyCommandBuild extends Building{

```

### `core/src/mindustry/world/blocks/liquid/Conduit.java`

```diff

+        public void drawCached(){

+        public void draw(boolean under){

-        protected void drawAt(float x, float y, int bits, int rotation, SliceMode slice){

+        protected void drawAt(float x, float y, int bits, int rotation, SliceMode slice, boolean under){

```

### `core/src/mindustry/world/blocks/logic/LogicBlock.java`

```diff

+        public @Nullable ObjectIntMap<String> linkMap;

+        private @Nullable Building optionalLink(String name){

```

### `core/src/mindustry/world/blocks/logic/LogicDisplay.java`

```diff

+    public Color backgroundColor = Pal.darkerMetal;

```

### `core/src/mindustry/world/blocks/power/HeaterGenerator.java`

```diff

+        public double sense(LAccess sensor){

```

### `core/src/mindustry/world/blocks/power/PowerGraph.java`

```diff

-    public int getId(){

```

### `core/src/mindustry/world/blocks/power/PowerNode.java`

```diff

+    public boolean useLod = true;

-        public void draw(){

+        public void drawCached(){

+        public void draw(){

```

### `core/src/mindustry/world/blocks/power/VariableReactor.java`

```diff

+        public double sense(LAccess sensor){

```

### `core/src/mindustry/world/blocks/production/HeatCrafter.java`

```diff

+        public double sense(LAccess sensor){

```

### `core/src/mindustry/world/blocks/sandbox/ItemSource.java`

```diff

+        public void configured(Unit builder, Object value){

```

### `core/src/mindustry/world/blocks/storage/Unloader.java`

```diff

+        public void configured(Unit builder, Object value){

```

### `core/src/mindustry/world/blocks/units/DroneCenter.java`

```diff

-public class DroneCenter extends Block{

-    public int unitsSpawned = 4;

-    public UnitType droneType;

-    public StatusEffect status = StatusEffects.overdrive;

-    public float droneConstructTime = 60f * 3f;

-    public float statusDuration = 60f * 2f;

-    public float droneRange = 50f;

-    public DroneCenter(String name){

-    public void init(){

-    public class DroneCenterBuild extends Building{

-        protected IntSeq readUnits = new IntSeq();

-        protected int readTarget = -1;

-        public Seq<Unit> units = new Seq<>();

-        public @Nullable Unit target;

-        public float droneProgress, droneWarmup, totalDroneProgress;

-        public void updateTile(){

-        public void drawConfigure(){

-        public void draw(){

-        public void write(Writes write){

-        public void read(Reads read, byte revision){

-    public class EffectDroneAI extends AIController{

-        public void updateUnit(){

```

### `core/src/mindustry/world/blocks/units/Reconstructor.java`

```diff

+    public void checkContentArrayCapacity(int items, int liquids){

```

### `core/src/mindustry/world/blocks/units/UnitAssembler.java`

```diff

+    public void checkContentArrayCapacity(int items, int liquids){

+        public boolean acceptUnitPayload(Unit unit){

```

### `core/src/mindustry/world/blocks/units/UnitFactory.java`

```diff

+    public void checkContentArrayCapacity(int items, int liquids){

```

### `core/src/mindustry/world/draw/DrawBlock.java`

```diff

-public abstract class DrawBlock{

+public class DrawBlock{

```

### `core/src/mindustry/world/modules/ItemModule.java`

```diff

-    private static final int windowSize = 6;

+    private static final int windowSize = 12;

-    private static final float pollScl = 20f;

+    private static final float pollScl = 10f;

+    public void checkArrayCapacity(int size){

```

### `core/src/mindustry/world/modules/LiquidModule.java`

```diff

-    private static final int windowSize = 3;

+    private static final int windowSize = 6;

-    private static final float pollScl = 20f;

+    private static final float pollScl = 10f;

+    public void checkArrayCapacity(int size){

```

### `desktop/src/mindustry/desktop/DesktopLauncher.java`

```diff

-    public void showFileChooser(boolean open, String title, String extension, Cons<Fi> cons){

-    public void showMultiFileChooser(Cons<Fi> cons, String... extensions){

+    public void showFileChooser(FileChooserParams params){

```

### `desktop/src/mindustry/desktop/steam/SNet.java`

```diff

+        public void sendStreamAsync(Streamable stream, ByteArrayOutputStream data){

```

### `server/src/mindustry/server/ServerControl.java`

```diff

-    private Fi patchDirectory;

-    private Seq<String> contentPatches = new Seq<>();

+    private Fi dataAssetDirectory, rulesFile;

+    private Seq<DataAsset> dataAssets = new Seq<>();

+    private LineReader lineReader;

```
