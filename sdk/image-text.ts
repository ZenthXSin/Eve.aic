import type { Json } from "../kernel/protocol.ts";

/** 文本视图保留出处；图像字节由宿主从原始 phone.image 回执另作模型图像输入。 */
export function imageTextView(value: Json): Json {
  if (Array.isArray(value)) return value.map(imageTextView);
  if (value === null || typeof value !== "object") return value;
  const result: Record<string, Json> = {};
  const image = typeof value.dataUrl === "string" && value.provenance !== null &&
    typeof value.provenance === "object" && !Array.isArray(value.provenance) &&
    value.provenance.kind === "phone_attachment";
  for (const [key, item] of Object.entries(value)) {
    if (image && key === "dataUrl") {
      result.imageInput = "只有明确选择 original 才由宿主提供原图输入；base64 不放入文字";
    } else result[key] = imageTextView(item);
  }
  return result;
}
