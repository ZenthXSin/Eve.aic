# DeepSpace English Bundle PR

> Source: Local work on `project/DeepSpace`, upstream `https://github.com/Icexuegao/DeepSpace`, 2026-05-22.

DeepSpace currently had `assets/bundles/bundle_zh_CN.properties` but no base English `assets/bundles/bundle.properties`. Since Mindustry/Arc loads mod bundles into `Core.bundle` by locale and falls back through the bundle parent chain, adding a base `bundle.properties` provides English/default strings without changing Chinese translations.

## Verification Sources

- `mindustry.Vars` (`源码和示例/Mindustry/core/src/mindustry/Vars.java`) loads the base internal `bundles/bundle` with locale resolution.
- `mindustry.mod.Mods` (`源码和示例/Mindustry/core/src/mindustry/mod/Mods.java`) adds mod bundle keys into every bundle in the parent chain.
- `mindustry.mod.ContentParser` (`源码和示例/Mindustry/core/src/mindustry/mod/ContentParser.java`) uses `type.content-name.name/description` entries and inserts JSON fallback values only when a key is absent.
- Example mods such as BetaMindy and NewHorizon ship `assets/bundles/bundle.properties` as the English/default bundle.

## Work Done

- Added `assets/bundles/bundle.properties` with 458 entries matching the Chinese key order exactly.
- Preserved all placeholders such as `{0}` and `{1}`.
- Preserved Mindustry rich text tags such as `[accent]`, `[lightgray]`, and newline escapes `\n`.
- Verified no CJK characters remain in the English bundle.

## Related

- [bundle_zh_CN.properties key prefix fix](bundle-zh-cn-key-prefix-fix.md) — previous note about Mindustry bundle key matching.
- [Mindustry Content Loading Chain](../concepts/mindustry-content-loading-chain.md) — content loading context.
