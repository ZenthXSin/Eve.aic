# 继承自 eve-portable 的 Mindustry JSON 校验规则

本目录是一份外部参考资源的固定副本，**不是本仓库 MIT 许可的一部分**。

- 来源：https://gitee.com/zenthxsin/eve-portable
- 版本：`e7e43b4db4de38775a8d28f59dd4ec69e5e1686e`
- 许可证：AGPL-3.0（见同目录 `LICENSE`，随副本一起提供）

| 文件 | 说明 |
| --- | --- |
| `vendor.py` | 上游 `tools/mindustry_json_validator.py` 的逐字节副本，sha256 `0c7bdf382f5b4c690b7e04e765e272d1d45c2d6552d461398df043a609e11ba7`。规则本身是 `ContentParser.java`（v8 / build 251）的手写复刻。 |
| `eve_modcheck.py` | Eve 写的调用层：把 Mindustry 的 hjson 写法展开成 JSON 后调用 `vendor.py`，输出一行 JSON。不含校验规则。 |

装配时由 `crates/practice-mindustry/src/rules.rs` 核对两个文件的 SHA-256，对不上就拒绝启用。解释器只在运行时执行这两个文件，它们不会被链接进任何可执行文件。

用法：

```sh
python3 scripts/check_mindustry_json.py <模组目录>
```

说明与验证证据见 `docs/继承-eve-portable-json校验.md`。
