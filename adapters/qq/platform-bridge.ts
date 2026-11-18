import type {
  PhonePlatformCapabilities,
  PhonePlatformCapabilitiesArgs,
  PhonePlatformCapability,
  PhonePlatformEvents,
  PhonePlatformEventsArgs,
} from "../../contracts/phone-platform.ts";
import { build, listPlatformOperations, lookup } from "./platform-catalog.ts";
import { createQqPlatformRead, type QqPlatformRequest } from "./platform-client.ts";

export const qqOperationRoutes = { lookup, build };

export function discoverPlatform(
  args: PhonePlatformCapabilitiesArgs = {},
): PhonePlatformCapabilities {
  if (
    !args || typeof args !== "object" || Array.isArray(args) ||
    Object.keys(args).some((key) => !["operation", "after", "limit"].includes(key))
  ) {
    throw new TypeError("invalid capability query");
  }
  const detail = Object.hasOwn(args, "operation");
  if (detail && (typeof args.operation !== "string" || Object.keys(args).length !== 1)) {
    throw new TypeError("invalid capability detail");
  }
  const page = detail
    ? { items: [lookup(args.operation!)], nextOffset: null }
    : listPlatformOperations(
      args.after === undefined ? 0 : args.after,
      args.limit === undefined ? 16 : args.limit,
    );
  const items = page.items.map((entry): PhonePlatformCapability => {
    const { name, access, method } = entry;
    const fields = detail
      ? {
        path: entry.path,
        surface: entry.surface,
        pathParams: entry.pathParams,
        queryParams: entry.queryParams,
        bodyParams: entry.bodyParams,
        requiredQuery: entry.requiredQuery,
        requiredBody: entry.requiredBody,
      }
      : {};
    if (name === "getMe" && access === "read" && method === "GET") {
      return { operation: name, access, method, chatReadable: true, ...fields };
    }
    return { operation: name, access, method, chatReadable: false, ...fields };
  });
  return page.nextOffset === null ? { items } : { items, nextAfter: page.nextOffset };
}

export function phonePlatformTools(
  request: QqPlatformRequest,
  page: (args: PhonePlatformEventsArgs) => Promise<PhonePlatformEvents>,
) {
  return {
    read: createQqPlatformRead({ catalog: qqOperationRoutes, request }),
    events: async (args: PhonePlatformEventsArgs): Promise<PhonePlatformEvents> => {
      const { items, throughSeq, hasMore } = await page(args);
      return {
        items: items.map(({ seq, type, receivedAt }) => ({ seq, type, receivedAt })),
        throughSeq,
        hasMore,
      };
    },
  };
}
