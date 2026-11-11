// 最小的 SemVer 比较，只支持内核需要的范围写法：
// "*"、"1.2.3"、"^1.2.3"、"~1.2.3"、">=1.2.3"。

export type Version = [number, number, number];

export function parseVersion(text: string): Version {
  const match = /^(\d+)\.(\d+)\.(\d+)$/.exec(text.trim());
  if (!match) throw new Error(`不是合法的版本号：${text}`);
  return [Number(match[1]), Number(match[2]), Number(match[3])];
}

export function compareVersions(a: Version, b: Version): number {
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return 0;
}

export function satisfies(version: string, range: string): boolean {
  const v = parseVersion(version);
  const r = range.trim();
  if (r === "*") return true;
  if (r.startsWith(">=")) return compareVersions(v, parseVersion(r.slice(2))) >= 0;
  if (r.startsWith("^")) {
    const base = parseVersion(r.slice(1));
    if (compareVersions(v, base) < 0) return false;
    if (base[0] > 0) return v[0] === base[0];
    if (base[1] > 0) return v[0] === 0 && v[1] === base[1];
    return v[0] === 0 && v[1] === 0 && v[2] === base[2];
  }
  if (r.startsWith("~")) {
    const base = parseVersion(r.slice(1));
    return compareVersions(v, base) >= 0 && v[0] === base[0] && v[1] === base[1];
  }
  return compareVersions(v, parseVersion(r)) === 0;
}
