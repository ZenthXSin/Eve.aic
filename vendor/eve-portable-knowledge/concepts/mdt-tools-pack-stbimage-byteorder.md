# MDT tools:pack STBImage 字节序通道错乱

> Source: 2026-06-17 群聊题目；源码：`Arc/arc-core/src/arc/graphics/Pixmap.java`（loadJni native 实现）

## 问题

在 MDT 的 `tools:pack` 中，使用 LWJGL 的 `STBImage.stbi_load_from_memory()` 加载图像后，有时像素的 RGBA 通道顺序会错乱（比如 alpha 和 red 互换）。

## 根因

`Pixmap.loadJni()` 的 native 实现使用 `stbi_load_from_memory(..., STBI_rgb_alpha)` 加载图像，返回的 ByteBuffer 通过 `NewDirectByteBuffer` 创建，**未显式设置字节序**，默认为 BIG_ENDIAN。

- STBImage 不受系统字节序影响，总是按固定顺序输出像素字节：`[R, G, B, A]`（每个像素 4 字节）
- 在小端（little-endian）系统上，`stbi_load_from_memory` 输出的 `[R, G, B, A]` 在内存中就是这个顺序
- 但 Java ByteBuffer 默认 BIG_ENDIAN，`getInt()` 会把第一个字节当作最高有效字节，返回 `0xRGBA`
- 而 `blend` 方法期望的颜色格式中，A 在最低字节（0-7 位），R 在最高字节（24-31 位）
- 当 ByteBuffer 字节序与系统原生字节序不匹配时，`getInt()` 和 `putInt()` 的字节序就会与 STBImage 输出不一致，导致 R 和 A 互换

## 修复方案

在 `loadJni` 返回 ByteBuffer 后，设置字节序为 LITTLE_ENDIAN：

```java
pixels = loadJni(nativeData, encodedData, offset, len);
pixels.order(ByteOrder.LITTLE_ENDIAN); // 添加此行
```

同时需要调整 `blend` 方法的位操作以适配 ABGR 格式（LITTLE_ENDIAN 下 getInt 读取 `[R, G, B, A]` 返回 `0xABGR`）：

```java
// 调整前（假设 0xRGBA 格式）
int dst_r = (dst >>> 24) & 0xff;
int dst_g = (dst >>> 16) & 0xff;
int dst_b = (dst >>> 8) & 0xff;
int dst_a = dst & 0xff;

// 调整后（适配 0xABGR 格式）
int dst_a = (dst >>> 24) & 0xff;
int dst_b = (dst >>> 16) & 0xff;
int dst_g = (dst >>> 8) & 0xff;
int dst_r = dst & 0xff;
```

## 相关

- [Mindustry tools:pack 描边偏红问题](../analysis/mindustry-tools-pack-reddish-outline.md)
- `Pixmap.getRaw()` / `setRaw()` 直接使用 `getInt()` / `putInt()`，依赖 ByteBuffer 字节序
