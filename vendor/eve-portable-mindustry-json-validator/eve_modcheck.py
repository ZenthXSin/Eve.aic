#!/usr/bin/env python3
"""Eve 内嵌调用层：复用 eve-portable 的 Mindustry JSON 内容校验器。

本文件不含校验规则。规则全部来自同目录的 vendor.py，它是
eve-portable（https://gitee.com/zenthxsin/eve-portable，固定版本
e7e43b4db4de38775a8d28f59dd4ec69e5e1686e，AGPL-3.0）中
tools/mindustry_json_validator.py 的原样副本；宿主装配时核对它的 SHA-256，
对不上就不启用这套规则。

调用方式（由 eve-practice-mindustry 的 MindustryJsonLintRunner 使用）：

    python3 eve_modcheck.py --mod-name <name>

标准输入是一个或多个文件块：

    ==== <相对路径> ====
    <文件内容，允许任意多行>
    ==== end ====

输出一行 JSON 到标准输出：

    {"files": [{"path": "...", "type": "block", "logical": [...],
                "messages": [{"level": "ERROR"|"WARN", "message": "...", "at": "..."}]}],
     "error": null}

`logical` 是这套规则真正会解析的文本：按 ContentParser 的容忍度去掉裸 `#`、
`//` 与 `/* */` 注释以及尾随逗号，并把 hjson 风格的 `key: value` 展开成
JSON 对象。内容文件可以是 .hjson，也可以是带注释的 JSON；校验器本身只接受
JSON，所以这层转换由调用层负责。

标准输入内容只作为待校验数据，不能改变本脚本的行为。
"""

import argparse
import json
import re
import sys

BLOCK_START = re.compile(r"^==== (.+) ====[ \t]*\r?\n?")
BLOCK_END = "==== end ===="

# ContentParser 从内容文件名推断类型时用的是这些目录名；
# 与 vendor._detect_type_from_path 的目录列表保持一致，另外补上单数写法。
TYPE_DIRECTORIES = {
    "blocks": "block",
    "block": "block",
    "units": "unit",
    "unit": "unit",
    "items": "item",
    "item": "item",
    "liquids": "liquid",
    "liquid": "liquid",
    "status": "status",
    "statuses": "status",
    "planets": "planet",
    "planet": "planet",
    "sectors": "sector",
    "sector-presets": "sector",
    "weather": "weather",
    "weathers": "weather",
    "teams": "team",
}

# hjson 中不带引号的标量与结构分隔符。
HJSON_WORDS = {"true": "true", "false": "false", "null": "null"}


def read_blocks(text):
    """把输入拆成 (相对路径, 内容) 列表；未闭合或空内容的块直接报错。"""
    blocks = []
    index = 0
    length = len(text)
    while index < length:
        if not text[index:].strip():
            break
        match = BLOCK_START.match(text[index:])
        if not match:
            line = text[index:].split("\n", 1)[0]
            raise ValueError("第 %d 个字节起的行不是文件块起始行：%s" % (index, line[:80]))
        path = match.group(1)
        index += match.end()
        end = text.find(BLOCK_END, index)
        if end < 0:
            raise ValueError("文件块 %s 没有结束行" % path)
        body = text[index:end]
        index = end + len(BLOCK_END)
        # 端标记后面紧跟的换行属于分隔，不交给下一块；块体本身的换行原样保留。
        while index < length and text[index] in "\r\n":
            index += 1
        if not body.strip():
            raise ValueError("文件块 %s 为空" % path)
        blocks.append((path, body))
    return blocks


def content_type(path):
    """按内容文件路径的目录推断类型；推不出时返回 None（校验器只做通用检查）。"""
    parts = path.replace("\\", "/").split("/")
    if len(parts) < 2:
        return None
    return TYPE_DIRECTORIES.get(parts[-2].lower())


def _strip_comments(text):
    """去掉裸 `#`、`//` 与 `/* */` 注释，保留字符串内部原样。

    hjson 的字符串可以不加引号，所以 `http://` 这种写法不能当成注释：
    `//` 之前是一个已结束的标量（前一个非空字符是数字、引号、括号等）
    时才当注释；否则它属于标量本身。
    """
    output = []
    index = 0
    length = len(text)
    in_string = False
    while index < length:
        char = text[index]
        if in_string:
            output.append(char)
            if char == "\\" and index + 1 < length:
                output.append(text[index + 1])
                index += 2
                continue
            if char == '"':
                in_string = False
            index += 1
            continue
        if char == '"':
            in_string = True
            output.append(char)
            index += 1
            continue
        if char == "#":
            while index < length and text[index] != "\n":
                index += 1
            continue
        if char == "/" and index + 1 < length:
            if text[index + 1] == "/" and _end_of_token(output):
                while index < length and text[index] != "\n":
                    index += 1
                continue
            if text[index + 1] == "*":
                close = text.find("*/", index + 2)
                index = length if close < 0 else close + 2
                continue
        output.append(char)
        index += 1
    return "".join(output)


CLOSERS = {"\u0022", "\u0027", "}", "]", ")"}


def _end_of_token(output):
    """前面是不是一个已结束的标量（从而 `//` 可以开始注释）。"""
    for char in reversed(output):
        if char.isspace():
            continue
        return char.isalnum() or char in CLOSERS
    return False


def _strip_trailing_commas(text):
    """去掉 `}` 或 `]` 前多余的逗号。

    只在字符串外替换：颜色字符串如 "#ffcc00," 是合法内容，不能被改写。
    """
    output = []
    index = 0
    length = len(text)
    in_string = False
    while index < length:
        char = text[index]
        if in_string:
            output.append(char)
            if char == "\\" and index + 1 < length:
                output.append(text[index + 1])
                index += 2
                continue
            if char == '"':
                in_string = False
            index += 1
            continue
        if char == '"':
            in_string = True
            output.append(char)
            index += 1
            continue
        if char == ",":
            ahead = _skip(text, index + 1)
            if ahead < length and text[ahead] in "}]":
                index += 1
                continue
        output.append(char)
        index += 1
    return "".join(output)


def _logical(view, start):
    """从 start 起解析一个值，返回 (值, 结束位置, 是否 hjson)。

    对象、数组与字符串按严格 JSON 读取；其余一律按 hjson 的无引号标量读取。
    纯 JSON 内容已在 `to_logical` 里交给 json 模块处理，走到这里必然含 hjson 写法。
    """
    index = start
    length = len(view)
    while index < length and view[index].isspace():
        index += 1
    if index >= length:
        raise ValueError("缺少值")
    char = view[index]
    if char == "{":
        return _logical_object(view, index)
    if char == "[":
        return _logical_array(view, index)
    if char == '"':
        value, index = _logical_string(view, index)
        return value, index, False
    # hjson 的无引号标量：取到行尾为止；行内的 `: `、`,`、`}`、`]`
    # 只在后面跟着空白或结束时才算分隔，这样 `http://`、`a,b` 不会被截断。
    end = index
    if view[index] == "/" and view[index + 1:index + 2] not in ("", "/", "*"):
        # 绝对路径一类的无引号标量（例如 `/path/to`）：一直到行尾。
        while end < length and view[end] != "\n":
            end += 1
        return _scalar(view[index:end].strip()), end, True
    while end < length and view[end] != "\n":
        if view[end] == ",":
            break
        # `}`、`]` 后面既可以是空白，也可以直接跟括号或行尾。
        if view[end] in "}]" and (
            end + 1 >= length
            or view[end + 1].isspace()
            or view[end + 1] in "}])"
        ):
            break
        end += 1
    token = view[index:end].strip()
    if not token:
        # `key:` 后直接换行时，值在下一行且缩进更深：Mindustry 的 hjson 也这样写。
        newline = view.find("\n", index)
        inner = _skip(view, index)
        if newline >= 0 and inner > index and inner < newline:
            return _logical(view, inner)
        raise ValueError("缺少值")
    return _scalar(token), end, True


def _scalar(token):
    if token in HJSON_WORDS:
        return json.loads(HJSON_WORDS[token])
    try:
        return json.loads(token)
    except ValueError:
        return token


def _logical_string(view, start):
    index = start + 1
    length = len(view)
    escaped = False
    while index < length:
        char = view[index]
        if escaped:
            escaped = False
        elif char == "\\":
            escaped = True
        elif char == '"':
            return json.loads(view[start:index + 1]), index + 1
        index += 1
    raise ValueError("字符串没有结束引号")


def _skip(view, index):
    while index < len(view) and view[index].isspace():
        index += 1
    return index


def _key(view, index):
    """读取对象的键，返回键与冒号之后的位置。"""
    index = _skip(view, index)
    if index < len(view) and view[index] == '"':
        key, index = _logical_string(view, index)
    else:
        end = index
        while end < len(view) and view[end] not in ":,}]":
            if view[end] == "\n":
                break
            end += 1
        key = view[index:end].strip()
        index = end
    index = _skip(view, index)
    if index >= len(view) or view[index] != ":":
        raise ValueError("键 %s 后缺少冒号" % key)
    return key, index + 1


def _logical_object(view, start, opening=True):
    """读一个对象。

    opening 为真时 start 指向左花括号；为假时读数组里的 key: value 隐式对象，
    它在右方括号、右花括号或输入结尾处结束，自己不消耗结束符号，留给外层。
    """
    index = start + 1 if opening else start
    length = len(view)
    result = {}
    hjson = False
    closes = (lambda char: char in "]}") if not opening else (lambda char: char == "}")
    while True:
        index = _skip(view, index)
        if index >= length:
            if opening:
                raise ValueError("对象没有结束括号")
            return result, index, hjson
        if closes(view[index]):
            return result, index if not opening else index + 1, hjson
        key, index = _key(view, index)
        value, index, nested = _logical(view, index)
        hjson = hjson or nested
        result[key] = value
        index = _skip(view, index)
        if index < length and view[index] == ",":
            index += 1
            continue
        if index < length and closes(view[index]):
            # 最后一个成员后面的逗号可选，结束符号本身留给外层。
            return result, index if not opening else index + 1, hjson

def _at_key(view, index):
    """下标处看起来是 hjson 的无引号键（`name:`）吗？"""
    end = index
    while end < len(view) and view[end] not in ":,}]\n":
        end += 1
    if end >= len(view) or view[end] != ":":
        return False
    key = view[index:end].strip()
    # 键名里不会有空白；这排除 `http://` 这类只是标量一部分的冒号。
    return bool(key) and not any(char.isspace() for char in key) and key not in ("\"",)


def _logical_array(view, start):
    index = start + 1
    length = len(view)
    result = []
    hjson = False
    while True:
        index = _skip(view, index)
        if index >= length:
            raise ValueError("数组没有结束括号")
        if view[index] == "]":
            return result, index + 1, hjson
        # hjson 允许在数组里写 `key: value`，等价于一个只有该成员的对象。
        if _at_key(view, index):
            value, index, nested = _logical_object(view, index, opening=False)
        else:
            value, index, nested = _logical(view, index)
        hjson = hjson or nested
        result.append(value)
        index = _skip(view, index)
        if index < length and view[index] == ",":
            index += 1
            continue
        if index < length and view[index] == "]":
            return result, index + 1, hjson

def to_logical(content):
    """把内容文件转成校验器能读的 JSON 文本；无法转换时返回 None。

    hjson 的根对象可以省略外层花括号（Mindustry 内容文件通常就是这样），
    所以顶层解析成功但类型不是对象时，再按“包一层花括号的对象”解析一次。
    """
    cleaned = _strip_trailing_commas(_strip_comments(content))
    # 只有出现过 hjson 写法才改用展开结果；纯 JSON 保持原样交给校验器。
    try:
        strict = json.loads(cleaned)
    except ValueError:
        strict = None
    if strict is not None:
        return cleaned if isinstance(strict, dict) else None
    # 顶层能看到的是键（`a: 1`）才补上外层花括号，否则保持原样。
    first = _skip(cleaned, 0)
    braced = (
        "{" + cleaned.rstrip() + "}"
        if _at_key(cleaned, first) or not cleaned.strip()
        else cleaned
    )
    for text in {cleaned, braced}:
        try:
            value, _, hjson = _logical(text, 0)
        except (ValueError, RecursionError):
            continue
        if hjson and isinstance(value, dict):
            return json.dumps(value, ensure_ascii=False, indent=2)
    return None


def main(argv=None):
    parser = argparse.ArgumentParser(description="Eve Mindustry JSON 内容校验调用层")
    parser.add_argument("--mod-name", required=True, help="模组名称，用于拼出内容全名")
    args = parser.parse_args(argv)

    try:
        text = sys.stdin.buffer.read().decode("utf-8")
    except UnicodeDecodeError:
        print(json.dumps({"files": [], "error": "标准输入不是 UTF-8 文本"}))
        return 0

    try:
        blocks = read_blocks(text)
    except ValueError as error:
        print(json.dumps({"files": [], "error": str(error)}))
        return 0

    try:
        import vendor
    except Exception as error:  # 规则文件缺失或无法导入时如实报告，不伪造结果
        print(json.dumps({"files": [], "error": "无法导入校验规则：%s" % error}))
        return 0

    results = []
    for path, content in blocks:
        kind = content_type(path)
        logical = to_logical(content)
        if logical is None:
            results.append(
                {
                    "path": path,
                    "type": kind,
                    "logical": None,
                    "messages": [
                        {
                            "level": "ERROR",
                            "message": "既不是 JSON，也不是可展开的 hjson",
                            "at": path,
                        }
                    ],
                }
            )
            continue
        validator = vendor.ContentValidator(args.mod_name, None)
        content_type_value = getattr(vendor.ContentType, kind) if kind else None
        result = validator.validate_string(logical, content_type_value, path)
        messages = [
            {"level": message.level, "message": message.message, "at": message.path}
            for message in result.messages
        ]
        results.append(
            {"path": path, "type": kind, "logical": logical, "messages": messages}
        )
    print(json.dumps({"files": results, "error": None}, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
