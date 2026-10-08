"""Mindustry 无头服务端的确定性替身：按真实服务端的输出格式加载工作目录中的模组并回答控制台命令。

只用于离线验收运行器协议与宿主恢复语义，不代表真实游戏行为；真实验证见 CI 的 Mindustry 作业。
用法：python3 fake_mindustry.py [--record 文件] [--hold 文件] <JVM 参数...> -jar <jar>
--record 追加一行启动记录；--hold 在服务端就绪后等待该文件出现才处理命令（用于模拟运行中被终止）。
"""
import json
import os
import pathlib
import re
import sys
import time

BLOCK_TYPES = {"Wall", "Conveyor", "Drill", "Router", "Battery", "Door"}
ITEMS = {"copper", "lead", "graphite", "silicon", "titanium", "metaglass", "scrap", "sand", "coal"}
NULL = "@@eve:null@@"


def log(level, text):
    sys.stdout.write(f"\x1b[90m[10-08-2026 12:00:00]\x1b[0m [{level}] {text}\n")
    sys.stdout.flush()


def parse(text):
    values = {}
    for line in text.splitlines():
        match = re.match(r'\s*"?([A-Za-z]+)"?\s*:\s*(.*?)\s*,?\s*$', line)
        if match:
            values[match.group(1)] = match.group(2).strip('"')
    return values


def main():
    args = sys.argv[1:]
    record = hold = None
    while args and args[0] in ("--record", "--hold"):
        flag, value, args = args[0], args[1], args[2:]
        if flag == "--record":
            record = value
        else:
            hold = value
    assert "-jar" in args, "fake server must be launched like java -jar"
    if record:
        with open(record, "a", encoding="utf8") as file:
            file.write(json.dumps({"cwd": os.getcwd(), "args": args,
                                   "home": os.environ.get("HOME") or os.environ.get("USERPROFILE")}) + "\n")
    mods = {}
    errors = []
    for directory in sorted(pathlib.Path("config/mods").glob("*")):
        manifest = parse((directory / "mod.hjson").read_text(encoding="utf8"))
        name = manifest.get("name", directory.name)
        content = {}
        for path in sorted(directory.glob("content/*/*")):
            values = parse(path.read_text(encoding="utf8"))
            kind = path.parent.name
            relative = path.relative_to(pathlib.Path.cwd() if path.is_absolute() else pathlib.Path("."))
            default = "Block" if kind == "blocks" else kind[:-1].capitalize()
            declared = values.get("type", default)
            if kind == "blocks" and declared not in BLOCK_TYPES:
                log("W", f"[{relative.as_posix()}] No type '{declared}' found, defaulting to type 'Block'")
                declared = "Block"
            for item in re.findall(r"([a-z-]+)/\d+", values.get("requirements", "")):
                if item not in ITEMS:
                    errors.append(f"{path.name}: IllegalArgumentException: \"item\": No item found with name '{item}'.")
            values["class"] = declared
            content[f"{name}-{path.stem}"] = values
        mods[name] = {"display": manifest.get("displayName", name), "version": manifest.get("version", "1.0"),
                      "content": content}
    if errors:
        log("E", "Error occurred loading mod content:")
        for error in errors:
            log("E", f"| | {error}")
        log("E", "The server will now exit.")
        return 1
    log("I", f"{len(mods)} mods loaded.")
    log("I", "Server loaded. Type 'help' for help.")
    if hold:
        while not os.path.exists(hold):
            time.sleep(0.05)
    for line in sys.stdin:
        command = line.strip()
        if command == "exit":
            log("I", "Shutting down server.")
            return 0
        if command == "version":
            log("I", "Version: Mindustry fake / build 160.7")
        elif command == "mods":
            log("I", "Mods:")
            for mod in mods.values():
                log("I", f"  {mod['display']} {mod['version']}")
            log("I", f"Mod directory: {os.getcwd()}/config/mods")
        elif command.startswith("js "):
            log("I", evaluate(command[3:], mods))
        else:
            log("E", f"Invalid command. Type 'help' for help.")
    return 0


def evaluate(expression, mods):
    literal = re.fullmatch(r'"([^"]*)"', expression)
    if literal:
        return literal.group(1)
    mod = re.search(r'Vars\.mods\.getMod\("([a-z0-9-]+)"\)', expression)
    if mod:
        return "true" if mod.group(1) in mods else "false"
    subject = re.search(r'\}\)\("([a-z0-9-]+)"\)', expression).group(1)
    content = next((mod["content"][subject] for mod in mods.values() if subject in mod["content"]), None)
    if expression.startswith("String(") and expression.endswith("!=null)"):
        return "true" if content is not None else "false"
    if content is None:
        return NULL
    if "getSimpleName" in expression:
        return content["class"]
    field = re.search(r"c\.([A-Za-z]+)===undefined", expression).group(1)
    defaults = {"size": "1", "health": "40"}
    return content.get(field, defaults.get(field, NULL))


if __name__ == "__main__":
    sys.exit(main())
