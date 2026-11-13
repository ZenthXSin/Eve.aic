/** 子代理会话只读查看：左侧会话、右侧消息；手机上先选会话再进入消息。样式见 styles.ts。 */
export const agentsMarkup = String.raw`<section id="agentsPanel" class="view" hidden>
<div class="view-head"><p>Eve 委派给子代理的独立会话。这里只读查看，不会改变 Eve 的未读通知。</p><div class="row"><span id="agentsState" class="hint" role="status"></span><button id="agentsRefresh" class="small">刷新会话</button></div></div>
<div class="agent-layout" id="agentLayout" data-pane="list">
<div class="agent-list"><div id="agentList"></div><button id="agentsMore" class="small" hidden>更多会话</button></div>
<div class="agent-chat">
<div class="agent-chat-head"><button id="agentBack" class="link">← 全部会话</button><h3 id="agentTitle">选择一个会话</h3><p id="agentConfig" class="hint"></p></div>
<div class="agent-scroll"><button id="agentOlder" class="small" hidden>更早消息</button><div id="agentMessages"></div></div>
</div></div></section>`;

export const agentsScript = String.raw`
let agentsSelected=null,agentsListNext=null,agentsBefore=null,agentsGeneration=0;
const agentRoles={review:'审查',coding:'编码',investigation:'调查'};
const agentStatuses={queued:'已排队',running:'工作中',completed:'完成',failed:'失败',unknown:'结果未知',cancelled:'取消'};
async function agentQuery(operation,args){return await api('/api/query',{resource:'agents',operation,args});}
function agentPane(pane){el('agentLayout').setAttribute('data-pane',pane);}
function clearAgentPanel(){
  agentsGeneration++;agentsSelected=null;agentsListNext=null;agentsBefore=null;
  el('agentList').replaceChildren();el('agentMessages').replaceChildren();
  el('agentTitle').textContent='选择一个会话';el('agentConfig').textContent='';el('agentsState').textContent='';
  agentPane('list');
}
async function refreshAgents(append=false){
  const session=csrf,generation=agentsGeneration;
  el('agentsState').textContent='同步中…';
  try{
    const value=await agentQuery('page',append?agentsListNext||{}:{});
    if(csrf!==session||generation!==agentsGeneration)return;
    if(!append)el('agentList').replaceChildren();
    for(const agent of value.items||[]){
      const state=agent.state==='working'?'工作中 · '+agent.pending+' 项排队':agent.state==='closed'?'已关闭':'空闲';
      const button=node('button','',agent.config.name+'\n'+(agentRoles[agent.config.role]||agent.config.role)+' · 思考强度 '+agent.config.reasoningEffort+'\n'+state+(agent.unread?' · '+agent.unread+' 条未读':''));
      button.setAttribute('aria-current',String(agentsSelected===agent.agentId));
      button.addEventListener('click',()=>run(()=>openAgentPanel(agent,button)));
      el('agentList').append(button);
    }
    if(!append&&!(value.items||[]).length)el('agentList').append(node('p','hint','还没有子代理会话。'));
    agentsListNext=value.next||null;
    el('agentsMore').hidden=!agentsListNext;
    el('agentsState').textContent='已同步';
    if(agentsSelected&&!append)await loadAgentMessages(false);
  }catch(error){
    if(csrf===session&&generation===agentsGeneration)el('agentsState').textContent='同步失败：'+error.message;
    throw error;
  }
}
async function openAgentPanel(agent,button){
  agentsGeneration++;agentsSelected=agent.agentId;agentsBefore=null;
  for(const button of el('agentList').children)button.setAttribute('aria-current','false');
  if(button)button.setAttribute('aria-current','true');
  el('agentTitle').textContent=agent.config.name;
  el('agentConfig').textContent='模型：'+(agent.config.model||'跟随共享模型')+' · 思考强度：'+agent.config.reasoningEffort+(agent.config.instructions?'\n'+agent.config.instructions:'');
  agentPane('chat');
  await loadAgentMessages(false);
}
async function loadAgentMessages(older){
  if(!agentsSelected)return;
  const session=csrf,id=agentsSelected,generation=agentsGeneration;
  const value=await agentQuery('page',{agentId:id,...(older&&agentsBefore?{beforeSeq:agentsBefore}:{})});
  if(csrf!==session||id!==agentsSelected||generation!==agentsGeneration)return;
  agentsBefore=value.older?.beforeSeq??null;
  el('agentOlder').hidden=!agentsBefore;
  const fragment=document.createDocumentFragment();
  for(const message of value.messages||[]){
    const article=node('article','agent-message');
    article.dataset.direction=message.direction;
    article.append(node('small','',(message.direction==='outgoing'?'Eve':'子代理')+' · '+time(message.at)+' · '+(agentStatuses[message.status]||message.status)));
    const body=node('p','',message.preview);
    article.append(body);
    if(message.truncated){
      const more=node('button','','展开完整消息 · '+message.characters+' 字');
      let next=0;
      more.addEventListener('click',()=>run(async()=>{
        const auth=csrf;
        const page=await agentQuery('get',{seq:message.seq,startChar:next,limit:8192});
        if(csrf!==auth||agentsSelected!==id||generation!==agentsGeneration)return;
        body.textContent=(next?body.textContent:'')+page.text;
        next=page.nextChar;
        more.hidden=next===undefined;
        more.textContent='继续读取这条消息';
      }));
      article.append(more);
    }
    fragment.append(article);
  }
  if(!(value.messages||[]).length&&!older)fragment.append(node('p','hint','这个会话还没有消息。'));
  if(older)el('agentMessages').prepend(fragment);else el('agentMessages').replaceChildren(fragment);
}
el('agentsRefresh').addEventListener('click',()=>run(()=>refreshAgents()));
el('agentsMore').addEventListener('click',()=>run(()=>refreshAgents(true)));
el('agentOlder').addEventListener('click',()=>run(()=>loadAgentMessages(true)));
el('agentBack').addEventListener('click',()=>agentPane('list'));
`;
