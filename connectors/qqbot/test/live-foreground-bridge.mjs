// 生产 bridge-core 的真实进程夹具；原始入站与平台出口均为本地替身。
import readline from "node:readline";
import fs from "node:fs";
import path from "node:path";
import {createBridge} from "../bridge-core.mjs";
const directory=process.argv[2];
const handlers=new Map();
const record=value=>fs.appendFileSync(path.join(directory,"bridge.jsonl"),JSON.stringify(value)+"\n");
let sent=0;
const bot={appId:"app",on:(name,handler)=>handlers.set(name,handler),stop(){},async sendText(target,text){
  record({type:"platform",target,text});
  fs.writeFileSync(path.join(directory,"platform-started"),"started");
  if (fs.existsSync(path.join(directory,"delay-receipt"))) {
    await new Promise((resolve,reject)=>{
      const deadline=Date.now()+10_000;
      const waiting=setInterval(()=>{
        if (fs.existsSync(path.join(directory,"release-receipt"))) {clearInterval(waiting);resolve();}
        else if (Date.now()>=deadline) {clearInterval(waiting);reject(new Error("fixture receipt timed out"));}
      },10);
    });
  }
  return {id:`platform-${++sent}`};
}};
const bridge=createBridge(bot,frame=>{record(frame);process.stdout.write(JSON.stringify(frame)+"\n");});
const incoming=(id,text)=>handlers.get("message")({}, {kind:"c2c",messageId:id,senderId:"alice",content:text,replyTarget:{scope:"c2c",targetId:"alice",msgId:id}});
handlers.get("ready")();
incoming("initial-input","继续执行这个任务");
let delivered=false;
let queuedBeforeSdk=null;
const dispatch=frame=>void bridge.command(frame).then(()=>{if(frame.type==="stop"){clearInterval(timer);input.close();process.exit(0);}});
const timer=setInterval(()=>{
  if (!delivered && !fs.existsSync(path.join(directory,"original-progress-only")) && fs.existsSync(path.join(directory,"tools-ready"))) {
    delivered=true;incoming("later-input","你还在吗？先和我说说现在的情况");
  }
  if (queuedBeforeSdk && fs.existsSync(path.join(directory,"release-before-sdk"))) {
    const queued=queuedBeforeSdk;queuedBeforeSdk=null;
    // 同一管道批次的控制帧可在 SDK 调用的 microtask 前完成；显式闸门不靠调度概率。
    for (const frame of queued) dispatch(frame);
  }
},10);
const input=readline.createInterface({input:process.stdin});
input.on("line",line=>{
  const frame=JSON.parse(line);record({direction:"host",...frame});
  if (frame.type==="passive_push" && frame.foreground===true &&
      fs.existsSync(path.join(directory,"delay-before-sdk"))) {
    queuedBeforeSdk=[];
    fs.writeFileSync(path.join(directory,"push-before-sdk"),"received");
  }
  if (queuedBeforeSdk && frame.type!=="stop") {
    queuedBeforeSdk.push(frame);
    if (frame.type==="finish" && frame.id==="initial-input") {
      fs.writeFileSync(path.join(directory,"original-finish-before-sdk"),"received");
    }
    return;
  }
  dispatch(frame);
});
