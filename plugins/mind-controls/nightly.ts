import { KernelError } from "../../kernel/protocol.ts";

export interface NightlySelfCheck {
  timeZone: string;
  hour: number;
  minute: number;
  attachmentDirectory: string;
}

/** Only schedules a reminder; cleanup remains an action chosen and performed by Eve. */
export function nightlySchedule(config: NightlySelfCheck) {
  if (
    !config || typeof config.timeZone !== "string" ||
    !Number.isInteger(config.hour) || config.hour < 0 || config.hour > 23 ||
    !Number.isInteger(config.minute) || config.minute < 0 || config.minute > 59 ||
    typeof config.attachmentDirectory !== "string" ||
    !config.attachmentDirectory.startsWith("/") || config.attachmentDirectory.includes("\0")
  ) throw new KernelError("rejected", "每晚自检需要有效时区、时分与附件临时目录");
  let format: Intl.DateTimeFormat;
  try {
    format = new Intl.DateTimeFormat("en-CA", {
      timeZone: config.timeZone,
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      hourCycle: "h23",
    });
  } catch {
    throw new KernelError("rejected", "每晚自检时区无效");
  }
  const local = (time: number) => {
    const fields = Object.fromEntries(format.formatToParts(time).map((p) => [p.type, p.value]));
    return {
      date: `${fields.year}-${fields.month}-${fields.day}`,
      hour: Number(fields.hour),
      minute: Number(fields.minute),
    };
  };
  // Bounded minute search also handles DST and time zones with non-hour offsets.
  const find = (time: number, direction: 1 | -1, skipDate?: string) => {
    const start = Math.floor(time / 60_000) * 60_000 + (direction === 1 ? 60_000 : 0);
    for (let i = 0; i < 60 * 73; i++) {
      const at = start + direction * i * 60_000, fields = local(at);
      if (
        fields.hour === config.hour && fields.minute === config.minute &&
        fields.date !== skipDate
      ) return { at: new Date(at).toISOString(), date: fields.date };
    }
    throw new KernelError("unavailable", "无法确定下次每晚自检时间");
  };
  return {
    signature: JSON.stringify(config),
    next: (time: number, skipDate?: string) => find(time, 1, skipDate),
    latest: (time: number) => find(time, -1),
    text:
      `每晚自检：查看附件临时目录 ${config.attachmentDirectory}。先把还要长期使用的附件移到工作区的非 tmp 目录，再用现有文件与终端工具清理已完成、未使用且超过24小时的普通附件文件；保留最近24小时、正在下载或使用的文件，跳过子目录、符号链接和 .part 临时文件，不递归清空 tmp。只清这里的可再下载附件，不动消息、下载回执或其他业务数据。根据真实结果留一句简短自检记录；没清理就如实说明。`,
  };
}
