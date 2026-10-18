# Mindustry BulletType and UnitType Content Pattern

> Source: Local source analysis of `mindustry.mod.ContentParser`, `mindustry.entities.bullet.BasicBulletType`, `mindustry.type.UnitType`, `mindustry.type.Weapon`, and example mods `ProjectUnityPrivate_V8` and `BetaMindy`.

## Conclusion

`BulletType` and `UnitType` are runtime content classes, not pure data records. Mindustry’s content system expects them to be filled through public fields, then initialized and loaded in later passes.

## What the source shows

### `ContentParser`

`mindustry.mod.ContentParser` contains a dedicated parser for `BulletType`:

- string → `Bullets` field lookup
- array → `MultiBulletType`
- object → resolve `type`, then instantiate and `readFields(...)`
- fallback for `basic` → `BasicBulletType`

This means JSON is a front-end for the same object model, not a separate schema language.

### `BasicBulletType`

`mindustry.entities.bullet.BasicBulletType` is a concrete, field-driven bullet class:

- public fields define colors, size, spin, shrink, sprites, and regions
- `load()` resolves atlas regions
- `draw(Bullet)` renders from those fields

This is the common pattern for content bullets: construct, assign fields, let `load()` and `draw()` consume them.

### `UnitType`

`mindustry.type.UnitType` exposes a large public surface for:

- movement, drag, accel, health, armor
- range, environment flags, mining, building
- rendering and layer settings
- payload and AI-related behavior
- weapon lists through `public Seq<Weapon> weapons`

`UnitType` is initialized later in the content lifecycle, so field values are meant to be set before `init()` / `postInit()` / `load()`.

### `Weapon`

`mindustry.type.Weapon` shows how units compose attack behavior:

- `public BulletType bullet`
- reload, recoil, rotation, inaccuracy, sound, effect, and draw fields
- `UnitType` consumes these weapons as a list

So unit attack behavior is not a single field; it is a nested object graph.

## Example mod pattern

### `ProjectUnityPrivate_V8`

`unity.content.UnityUnitTypes` and `unity.content.units.MonolithUnitTypes` use the standard content style:

- declare many `public static UnitType` fields
- instantiate in one `load()` method
- fill fields directly in initializer blocks
- attach weapons, bullets, abilities, and AI controllers through composition

This is a canonical large-mod organization style.

### `BetaMindy`

`BetaMindy` continues the same approach for bullets: direct instantiation, immediate field assignment, and occasional override when fields are not enough.

## Practical reading rule

If you want to understand a unit or bullet definition, read it in this order:

1. `mindustry.mod.ContentParser` for how the object can be constructed from JSON
2. the concrete class such as `mindustry.entities.bullet.BasicBulletType`
3. the host container such as `mindustry.type.UnitType`
4. the composing object such as `mindustry.type.Weapon`
5. example mods for real-world field combinations

## Related

- [Mindustry Content Loading Chain](mindustry-content-loading-chain.md)
- [Mindustry JSON and Java Hybrid Mod Pattern](mindustry-json-and-java-hybrid-mod-pattern.md)
- [Mindustry JSON Mod 反序列化流程](mindustry-json-mod-deserialization.md)
- [Mindustry Unit System](../mindustry/unit-system.md)
- [Mindustry Content System](../mindustry/content-system.md)
