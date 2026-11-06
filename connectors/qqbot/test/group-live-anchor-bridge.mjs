// 真实 bridge-core 进程；逻辑时钟只用于离线复现群聊窗口过期。
import readline from "node:readline";
import fs from "node:fs";
import path from "node:path";
import {createBridge, PASSIVE_REPLY_WINDOW_MS} from "../bridge-core.mjs";

const directory = process.argv[2];
const handlers = new Map();
const record = value => fs.appendFileSync(path.join(directory,"bridge.jsonl"),JSON.stringify(value)+"\n");
const marker = name => path.join(directory,name);
let logicalNow = 0, sent = 0, advanced = false, added = false;
const bot = {appId:"app",on:(name,handler)=>handlers.set(name,handler),stop(){},
  async sendText(target,text) {record({type:"platform",target,text});return {id:`group-platform-${++sent}`};}};
const bridge = createBridge(bot,frame=>{record(frame);process.stdout.write(JSON.stringify(frame)+"\n");},128,()=>logicalNow);
const incoming = (id,text) => handlers.get("message")({}, {
  kind:"group",messageId:id,senderId:"alice",groupOpenid:"group-one",content:text,
  rawEventType:"GROUP_AT_MESSAGE_CREATE",replyTarget:{scope:"group",targetId:"group-one",msgId:id},
});
handlers.get("ready")();
const restarted = fs.existsSync(marker("started"));
fs.writeFileSync(marker(restarted ? "reopened" : "started"),"ready");
if (!restarted) incoming("original-group-input","继续核对这个长任务");
const timer = setInterval(()=>{
  if (restarted) return;
  if (!advanced && fs.existsSync(marker("expire-original"))) {
    advanced = true;logicalNow = PASSIVE_REPLY_WINDOW_MS + 1;
    fs.writeFileSync(marker("original-expired"),"expired");
  }
  if (!added && fs.existsSync(marker("accept-followup"))) {
    added = true;incoming("fresh-group-input","还在吗，继续告诉我结果");
  }
},5);
const input = readline.createInterface({input:process.stdin});
input.on("line",line=>{
  const frame=JSON.parse(line);record({direction:"host",...frame});
  void bridge.command(frame).then(()=>{
    if(frame.type==="stop"){clearInterval(timer);input.close();process.exit(0);}
  });
});
