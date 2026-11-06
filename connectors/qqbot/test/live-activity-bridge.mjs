// 真实 bridge-core 的本地平台替身；不打开网络，不生成模型文本。
import readline from "node:readline";
import fs from "node:fs";
import path from "node:path";
import {createBridge} from "../bridge-core.mjs";

const directory=process.argv[2], mode=process.argv[3];
const handlers=new Map();
const record=value=>fs.appendFileSync(path.join(directory,"bridge.jsonl"),JSON.stringify(value)+"\n");
let sent=0;
const bot={appId:"app",on:(name,handler)=>handlers.set(name,handler),stop(){},
  async sendText(target,text) {
    record({type:"platform",target,text});
    if(mode==="delayed" && text.startsWith("系统进度：")) {
      fs.writeFileSync(path.join(directory,"host-written"),"written");
      await new Promise(resolve=>{const poll=setInterval(()=>{
        if(fs.existsSync(path.join(directory,"explicit-queued"))) {clearInterval(poll);resolve();}
      },5);});
    }
    return {id:`platform-${++sent}`};
  }};
const bridge=createBridge(bot,frame=>{record(frame);process.stdout.write(JSON.stringify(frame)+"\n");});
const incoming=text=>handlers.get("message")({}, {kind:"c2c",messageId:"initial-input",senderId:"alice",content:text,
  replyTarget:{scope:"c2c",targetId:"alice",msgId:"initial-input"}});
handlers.get("ready")();
incoming(mode==="quiet" ? "这个先不用回答，安静核对" : "执行并核对这个任务");
const input=readline.createInterface({input:process.stdin});
input.on("line",line=>{
  const frame=JSON.parse(line);record({direction:"host",...frame});
  void bridge.command(frame).then(()=>{if(frame.type==="stop"){input.close();process.exit(0);}});
});
