# Crystal Works Kotlin Migration and Auto Release Workflow

> Source: Conversation/project work on 2026-05-25; verified against existing knowledge pages for Mindustry Kotlin mod structure and crystal-works v1.5.0 power distribution workflow.

## Summary

Crystal Works after v1.5.0 moved from “Kotlin JVM shell + HJSON content” toward a real Kotlin Mindustry mod: content registration is done in `src/crystalworks/content/*.kt`, while GitHub Actions owns Android/desktop combined packaging and Release publication.

## Practical Workflow

- Target version should use an existing Mindustry release dependency. For the current line, `v155.4` built locally; plain `v155` did not provide the needed release dependency.
- Local build should verify the desktop jar path first with `./gradlew clean jar`.
- Android/full deploy packaging can be delegated to GitHub Actions when local shell lacks Android SDK / `ANDROID_HOME`.
- Release workflow should trigger on `v*` tags, run `./gradlew deploy`, and publish the built mod jar to the matching GitHub Release.
- Do not upload `.sha256` files in future GitHub Releases unless explicitly requested; users prefer only the playable/downloadable package artifact.
- After the workflow is installed, manual Release upload is unnecessary except as an emergency fallback.

## Kotlin Mod Migration Rules

- Keep `mod.json` as the mod metadata entry and point `main` to the Kotlin `Mod` subclass.
- Use `loadContent()` as the registration entry and call content registries in deterministic order:
  1. items
  2. liquids
  3. status effects
  4. blocks
- Put real content into Kotlin registry objects such as `CWItems`, `CWLiquids`, `CWStatusEffects`, and `CWBlocks`.
- Delete legacy `content/*.hjson` once equivalent Kotlin content exists, otherwise duplicate content names or stale definitions become likely.
- Keep field references for cross-content dependencies instead of anonymous one-shot content creation.

## Anti-Homogenization Planning

Kotlin migration should not be just syntax conversion. The project plan should reserve mechanics that require JVM logic and give Crystal Works an identity distinct from generic “more blocks, bigger numbers” mods:

- resonance networks
- grid overload and discharge behavior
- crystal-fluid thermal cycles
- event-driven research guidance
- visual state driven by runtime logic
- charged counterattack defense blocks
- rule: every new block must introduce a new decision, not only a stronger stat line

## Related

- [Mindustry Kotlin Mod Structure](../concepts/mindustry-kotlin-mod-structure.md) — Kotlin entry class, `loadContent()` / `init()` split, object registries, and `mod.json` requirement.
- [Crystal Works Phase 6 Power Distribution](crystal-works-phase-6-power-distribution.md) — previous v1.5.0 power chain work and validation evidence.
- [crystal-works 验证流程与方法](../concepts/crystal-works-validation-flow.md) — server validation and release-side checks.
