# DeepSpace Liquids Hardcoded English Localization PR


DeepSpace liquids are registered in Kotlin in `src/ice/content/ILiquids.kt` using Mindustry `mindustry.type.Liquid` subclasses such as project-side `IceLiquid`, `CellLiquid`, and `ReactLiquid`.

The project already uses `universecore.ui.bundle.localization` with hardcoded blocks of the form:

```kotlin
localization {
  zh_CN {
    localizedName = "..."
    description = "..."
  }
  en {
    localizedName = "..."
    description = "..."
  }
}
```

For this workflow, English translations should be added directly beside the existing `zh_CN` blocks, not through `assets/bundles/bundle.properties`.

## Work Done

- Branched from upstream `origin/master` into `add-liquids-english-localization`.
- Added `en { ... }` hardcoded localization to all 26 existing liquid localization blocks in `src/ice/content/ILiquids.kt`.
- Added English `details` where the Chinese block already had `details`.
- Left hidden/render-only `孢子云` unchanged because it had no visible localization block.

## Verification

- Checked Mindustry source `源码和示例/Mindustry/core/src/mindustry/type/Liquid.java` to confirm liquids are `UnlockableContent` and expose localization-relevant `localizedName` / description metadata through the content system.
- Checked project examples:
  - `src/ice/content/IItems.kt` for existing `zh_CN` + `en` localization blocks.
  - `src/singularity/contents/LiquidBlocks.kt` for project-side localization DSL usage.
- Static validation passed:
  - 26 `zh_CN` blocks and 26 `en` blocks in `ILiquids.kt`.
  - Every English block has `localizedName` and `description`.
  - English blocks contain no CJK characters.
  - Kotlin string quote check passed.
  - `git diff --check` passed.
- `bash ./gradlew classes --no-daemon --stacktrace` was attempted but timed out in the environment before producing a build result.

## PR

- Branch: `ZenthXSin:add-liquids-english-localization`
- Commit: `536aacad Add English localization for liquids`
- PR: `https://github.com/Icexuegao/DeepSpace/pull/58`
- PR status after creation: open, mergeable, clean.

## Related

- [DeepSpace DefenseBlocks hardcoded English localization PR](deepspace-defenseblocks-hardcoded-english-pr.md) — same project-side hardcoded localization pattern for blocks.
- [DeepSpace English Bundle PR](deepspace-english-bundle-pr.md) — previous bundle-based localization attempt and why it differs from hardcoded localization requests.
- [Mindustry Mod 生命周期与解析器边界](../concepts/mindustry-mod-lifecycle-parser-boundary.md) — content loading and parser boundary context.
