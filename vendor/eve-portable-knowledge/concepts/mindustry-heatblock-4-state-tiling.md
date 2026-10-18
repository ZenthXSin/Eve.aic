# Mindustry HeatBlock 4-State Tiling

> Source: Conversation about implementing Conveyor-like multi-connection visuals for a HeatBlock with only 4 textures.

For a HeatBlock-like block that needs Conveyor-style multi-direction connections but only has 4 textures, the simplest workable approach is to encode the four neighbors into a 4-bit mask and map that mask into four visual states.

## Key Points

- Separate **connection logic** from **render logic**.
- `blends()` only answers whether a neighbor can connect; it does not choose the final texture.
- Cache directional connection state in `onProximityUpdate()` or compute it directly in `draw()` if simplicity matters more than performance.
- With only 4 textures, the most practical representation is a reduced state set, such as:
  - state 0: left-to-right / single-line style
  - state 1: up-right corner style
  - state 2: down-left-right style
  - state 3: cross style
- If the visual logic is based on four directions, use a bitmask like:
  - up = 1
  - right = 2
  - down = 4
  - left = 8
- Then convert the mask into one of the 4 available textures.
- If you want actual Conveyor-like visuals, consider drawing a base region plus directional overlays instead of trying to force all states into one region index.

## Related

- [Mindustry Placement/HUD UI 修改](mindustry-placement-ui-hud.md) — examples of block placement and HUD-related rendering patterns
- [Mindustry JSON 可用接口概览](mindustry-json-interfaces.md) — general interface and field reference for JSON-defined blocks
- [Mindustry Block 科技树 JSON 定义](mindustry-block-tech-tree-json.md) — example of block definition structure and placement-related metadata
- [Mindustry JSON 多液体输入](mindustry-json-multi-liquid-input.md) — pattern for handling multiple input conditions and state mapping
- [Block and Building System](../mindustry/block-building-system.md) — building-side behavior and update flow
- [Rendering System](../mindustry/rendering-system.md) — rendering pipeline concepts relevant to tiling and drawing
- [Mindustry JSON 工厂活塞运动动画](mindustry-json-piston-animation.md) — example of state-driven sprite motion and visual composition
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions
- [Mindustry 电弧特效](mindustry-electric-arc-effect.md) — effect drawing patterns that also rely on state-based visual decisions