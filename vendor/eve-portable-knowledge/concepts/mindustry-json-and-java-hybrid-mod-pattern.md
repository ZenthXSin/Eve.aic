# Mindustry JSON and Java Hybrid Mod Pattern

> Source: Local example mods `BetaMindy`, `ProjectUnityPrivate_V8`, and `MultiCrafterLib`, plus source analysis of `mindustry.mod.ContentParser` and `mindustry.core.ContentLoader`.

## Conclusion

Mindustry mods commonly mix Java and JSON instead of choosing only one style.

## Pattern

- **Java** is used for behavior, custom logic, and extension points.
- **JSON** is used for structured configuration and content definition when the standard parser can handle it.
- Both feed the same content registration pipeline.

## Observed Usage

- `BetaMindy` shows direct construction of bullet content and method overrides.
- `ProjectUnityPrivate_V8` concentrates unit and bullet definitions in content classes.
- `MultiCrafterLib` shows Java content setup alongside JSON-driven structures.

## Why It Works

- The content system is field-driven.
- Most important runtime objects are still Java classes.
- JSON is only a serialization front-end for those classes.

## Related

- [Mindustry Content Loading Chain](mindustry-content-loading-chain.md)
- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md)
- [Mindustry JSON 问题解决流程](json-problem-solving-flow.md)
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md)
