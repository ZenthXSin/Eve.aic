# Crystal Works image2 Sprite Rebuild

> Source: 2026-05-25 asset rebuild task; verified against `CWBlocks.kt`, Mindustry v155/v158 sprite loading conventions, and local build.

## Summary

Crystal Works currently has 45 PNG sprites under `project/crystal-works/sprites`. The rebuild preserved all filenames and required layer suffixes used by Mindustry loaders:

- `Drill`: `-rim`, `-rotator`, `-top`
- `BurstDrill`: `-top-invert`, `-glow`, `-arrow`, `-arrow-blur`
- `Conduit`: `-top-#`, `-bottom-#`, `-cap`
- `Battery` / generator drawers: `-top`, `-power`, `-glow`

## Verification

- Knowledge checked: Crystal Works Kotlin migration workflow and Mindustry art asset reference path.
- Project source checked: `CWBlocks.kt` block names, classes, sizes, drawer suffixes.
- Mindustry source checked: `Conduit.java`, `LiquidBridge.java`, `Battery.java`, `Drill.java`, `BurstDrill.java`.
- Example assets checked: local Mindustry raw sprites and sample mod sprite naming.
- PNG validation: 45 files, all RGBA, non-empty.
- Build validation: `./gradlew jar --no-daemon` succeeded.

## Outputs

- Backup of previous sprites: `tmp/crystal-works-sprites-before-image2`
- Rebuilt sprite archive: `tmp/crystal-works-image2-rebuild.zip`
- Preview sheet: `tmp/crystal-works-image2-rebuild/preview.png`
- Report: `tmp/crystal-works-image2-rebuild/sprite-report.txt`

## Related

- [Crystal Works Kotlin Migration and Auto Release Workflow](crystal-works-kotlin-migration-auto-release.md)
- [Mindustry Art Assets Reference Path](../concepts/mindustry-art-assets-reference-path.md)
