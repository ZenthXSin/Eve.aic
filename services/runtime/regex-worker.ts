/// <reference lib="webworker" />
// 不受信任的正则只在可终止的 worker 内执行；主线程逐份发送便签，避免一次复制全库。
let expression: RegExp | undefined;
self.onmessage = (event: MessageEvent<{ query?: string; text?: string }>) => {
  if (event.data.query !== undefined) {
    try {
      expression = new RegExp(event.data.query, "u");
      self.postMessage({ ready: true });
    } catch {
      self.postMessage({ invalid: true });
    }
  } else {
    self.postMessage({ matched: expression!.test(event.data.text!) });
  }
};
