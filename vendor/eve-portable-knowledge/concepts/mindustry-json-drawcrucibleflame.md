# DrawCrucibleFlame 模板

下面给出一个可直接套用的 Mindustry JSON 模板。

## 1. 最小模板
```json
{
  "type": "GenericCrafter",
  "name": "example-crucible",
  "drawer": {
    "type": "DrawCrucibleFlame"
  }
}
```

## 2. 带常见方块字段的模板
```json
{
  "type": "GenericCrafter",
  "name": "example-crucible",
  "size": 2,
  "health": 320,
  "category": "crafting",
  "requirements": [
    ["copper", 60],
    ["lead", 40]
  ],
  "consumes": {
    "power": 1.5,
    "item": {
      "items": ["copper/1"]
    }
  },
  "outputItems": [
    {
      "item": "slag",
      "amount": 1
    }
  ],
  "drawer": {
    "type": "DrawCrucibleFlame"
  }
}
```

## 3. 可直接改的实战模板
如果你要的是“能看起来更像熔炉火焰”的版本，可以直接从这个开始：
```json
{
  "type": "GenericCrafter",
  "name": "test-crucible",
  "size": 2,
  "health": 200,
  "category": "crafting",
  "drawer": {
    "type": "DrawCrucibleFlame",
    "flameColor": "ff7b4a",
    "midColor": "ffd37a",
    "flameRad": 5.5,
    "circleSpace": 1.2,
    "flameRadiusScl": 0.9,
    "flameRadiusMag": 0.7,
    "circleStroke": 1.1,
    "x": 0,
    "y": 0,
    "alpha": 1,
    "particles": 10,
    "particleLife": 45,
    "particleRad": 7,
    "particleSize": 1.4,
    "fadeMargin": 0.15,
    "rotateScl": 1,
    "particleInterp": "pow2"
  }
}
```

## 4. 参数怎么调
- 想要**火焰更大**：提高 `flameRad`
- 想要**火焰更跳**：提高 `flameRadiusMag`
- 想要**更密的粒子**：提高 `particles`
- 想要**粒子飘得更久**：提高 `particleLife`
- 想要**粒子更粗更明显**：提高 `particleSize`
- 想要**外圈更醒目**：提高 `circleStroke`
- 想要**更柔和**：降低 `alpha` 或减小 `flameRadiusMag`

## 5. 常见错误
- 把 `DrawCrucibleFlame` 写成顶层字段
- 放错到不支持 `drawer` 的 block 类型上
- `requirements` / `consumes` 结构写错

## 6. 适合你直接改的模板
如果你只是想先跑通，可以从这个开始：
```json
{
  "type": "GenericCrafter",
  "name": "test-crucible",
  "size": 2,
  "health": 200,
  "category": "crafting",
  "drawer": {
    "type": "DrawCrucibleFlame"
  }
}
```

