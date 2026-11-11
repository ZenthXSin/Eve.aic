/** 持久事实提交；临时文件、目标和目录均同步，失败时保留现场。 */
export async function writeDurableJson(path: string, value: unknown): Promise<void> {
  const temporary = `${path}.${crypto.randomUUID()}.tmp`;
  const file = await Deno.open(temporary, { createNew: true, write: true, mode: 0o600 });
  try {
    const bytes = new TextEncoder().encode(JSON.stringify(value) + "\n");
    let offset = 0;
    while (offset < bytes.length) {
      const written = await file.write(bytes.subarray(offset));
      if (written === 0) throw new Error("事实写入未推进");
      offset += written;
    }
    await file.sync();
  } finally {
    file.close();
  }
  await Deno.rename(temporary, path);
  const directory = await Deno.open(path.slice(0, path.lastIndexOf("/")), { read: true });
  try {
    await directory.sync();
  } finally {
    directory.close();
  }
}
