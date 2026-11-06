// 真实生产 bridge-core，无 SDK、凭据或网络；只替换平台 sendText。
import readline from "node:readline";
import fs from "node:fs";
import {createBridge} from "../bridge-core.mjs";
const handlers=new Map();
const record=value=>fs.appendFileSync(process.argv[2],JSON.stringify(value)+"\n");
let counter=0;
const bot={appId:"app",on:(event,callback)=>handlers.set(event,callback),stop(){},async sendText(target,text){record({type:"platform",target,text});return {id:`platform-${++counter}`};}};
const bridge=createBridge(bot,frame=>{record(frame);process.stdout.write(JSON.stringify(frame)+"\n");});
handlers.get("ready")();
handlers.get("message")({}, {kind:"c2c",messageId:"fresh-input",senderId:"alice",content:"接着聊",replyTarget:{scope:"c2c",targetId:"alice",msgId:"fresh-input"}});
const input=readline.createInterface({input:process.stdin});
input.on("line",line=>{
  const frame=JSON.parse(line);record({direction:"host",...frame});
  void bridge.command(frame).then(()=>{if(frame.type==="stop"){input.close();process.exit(0);}});
});
