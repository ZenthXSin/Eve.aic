# Crystal Works: `crystalWallLarge` lateinit crash — order-of-init fix

> Source: Android crash report (v1.7.0, beta 155.4) — `lateinit property crystalWallLarge has not been initialized`

## Root Cause

`CWBlocks.load()` had `crystalWall` / `crystalWallLarge` defined **after** `crystalAssembler`, but `crystalAssembler`'s `payload-wall` recipe references `crystalWallLarge` as both `icon` and `outputPayload`.

In `loadContent()`, Mindustry's `ContentLoader.createModContent()` calls content registration in declaration order per file. Since `crystalAssembler` was initialized before `crystalWallLarge`, the `lateinit` field was accessed before assignment — crashing on Android (server might skip content loading depending on mod load path).

## Fix

Moved `crystalWall` and `crystalWallLarge` initialization **before** `crystalAssembler` in `CWBlocks.kt`. Removed the duplicate definition at the end of the file.

## Validation

- `./gradlew clean jar` passed
- Server-side load test (server-release.jar + mod jar) showed no errors
- Committed as `a9eb359`, pushed to `main` triggering auto release

## Related

- [Mindustry Kotlin Mod Structure](../concepts/mindustry-kotlin-mod-structure.md) — `loadContent()` registration order rules
- [CWBlocks.kt](https://github.com/ZenthXSin/crystal-works/blob/main/src/crystalworks/content/CWBlocks.kt) — final fix