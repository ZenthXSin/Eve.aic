# Mindustry 原版自由视角（Free Camera）

## 开启方式

**设置 → 图像 → 勾选「分离摄像头」（Detach Camera）**

在游戏中也可以通过快捷键 `Binding.detachCamera` 切换（默认未绑定按键，需要在设置中手动绑定）。

## 源码机制

### 核心存储

`settings.getBool("detach-camera", false)` — 持久化到存档设置。

### 开关逻辑

**文件**: `core/src/mindustry/input/DesktopInput.java` 第 240-267 行

```java
boolean detached = settings.getBool("detach-camera", false);

// 按键切换
if(input.keyTap(Binding.detachCamera)){
    settings.put("detach-camera", detached = !detached);
    if(!detached){
        panning = false;
    }
    spectating = null;
}

// detached 状态下强制启用 panning
panning |= detached;
```

**关键**: `panning = true` 时，摄像机脱离玩家单位，由 WASD/鼠标自由控制。

### 摄像机移动

**文件**: `core/src/mindustry/input/DesktopInput.java` 第 271-276 行

```java
if(((player.dead() || state.isPaused() || detached) && !ui.chatfrag.shown()) ...){
    // WASD 和鼠标中键移动摄像机
    Core.camera.position.add(Tmp.v1.setZero()
        .add(Core.input.axis(Binding.moveX), Core.input.axis(Binding.moveY))
        .nor().scl(camSpeed));
}
```

`detached` 时，摄像机位置由玩家手动控制，不再跟随单位。

### 单位行为变化

**文件**: `core/src/mindustry/input/DesktopInput.java` 第 966 行

```java
if(settings.getBool("detach-camera")){
    Vec2 targetPos = camera.position;
    movement.set(targetPos).sub(player).limit(speed);
    // 单位会向摄像机位置移动，但不会飞到那里——只是"看向"摄像机方向
    if(player.within(targetPos, 15f)){
        movement.setZero();
        unit.vel.approachDelta(Vec2.ZERO, unit.speed() * unit.type().accel / 2f);
    }
}
```

分离摄像头后，单位本身不会停止——它会尝试向摄像机位置移动（`movement` 指向 `camera.position`），但 `within(targetPos, 15f)` 检查会让它在接近时停下来。实质上单位会在原地附近待命。

### 设置界面

**文件**: `core/src/mindustry/ui/dialogs/SettingsMenuDialog.java` 第 554 行

```java
graphics.checkPref("detach-camera", false);
```

在图像（Graphics）设置页中作为一个 checkbox 展示。

## Binding 定义

**文件**: `core/src/mindustry/input/Binding.java` 第 89 行

```java
detachCamera = KeyBind.add("detach_camera", KeyCode.unset),
```

`KeyCode.unset` 表示默认没有绑定按键，玩家需要手动在设置中指定。

## 与其他模式的区别

| 模式 | 摄像机 | 单位行为 | 触发方式 |
|------|--------|----------|----------|
| **自由视角** (detach-camera) | 自由移动 | 向摄像机位置移动 | 图像设置 checkbox |
| **暂停建造** (pauseBuilding) | 跟随玩家 | 正常 | 快捷键 |
| **观战** (spectating) | 跟随目标单位 | 由 AI 控制 | 点击死亡单位 |
| **调试显示** (debugHitboxes) | 跟随玩家 | 正常 | 快捷键 |

## 注意

- 分离摄像头是**持久化**的（`settings.put`），退出重进仍然生效
- 关闭时需取消勾选或再按一次绑定键，否则摄像机一直处于自由状态
- 单位在 detached 模式下不会完全静止，会向摄像机位置缓慢移动
