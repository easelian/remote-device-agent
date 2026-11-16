/// What marks an element as a control on the page itself. The reader and the
/// details of an element decide it with the same code.
pub(super) const MARKS: &str = include_str!("../marks.js");

// Runs only in the isolated world. No caller-supplied code, selectors or method names.
pub(super) const DETAILS: &str = concat!(
    "function(){\n const marks=",
    include_str!("../marks.js"),
    ";\n",
    r#" if(!this.isConnected||this.nodeType!==1)return {connected:false};
 const text=e=>{const w=document.createTreeWalker(e,NodeFilter.SHOW_TEXT);let s='',n;for(let i=0;i<32&&s.length<512&&(n=w.nextNode());i++)s+=n.nodeValue.slice(0,512-s.length);return s;};
 const attr=(e,n)=>(e.getAttribute(n)||'').slice(0,512);
 // What a reference means: the element with its own text, the tags and naming attributes of
 // three ancestors, and the text of the record it belongs to where the markup marks one. Other
 // text nearby, such as a clock or a progress value beside a button, is not part of it.
 const a=[];let p=this;for(let i=0;p&&i<4;i++,p=p.parentElement){a.push([p.tagName,attr(p,'id'),attr(p,'name'),attr(p,'role'),attr(p,'data-id'),attr(p,'aria-label'),i===0?text(p):'']);}
 const record=marks.record;
 let owner=null;
 for(let q=this.parentElement||this.getRootNode().host,i=0;q&&i<8;i++,q=q.parentElement||q.getRootNode().host){if(record(q)){owner=q;a.push(['record',text(q)]);break;}}
 // While a reader is at work, whether the record still shows what the reader reported of it.
 const reader=globalThis.__rdhBrowserReader,steady=!owner||!reader||reader.closed||reader.steady(owner);
 const r=this.getBoundingClientRect(),s=getComputedStyle(this),root=this.getRootNode();
 // Input is sent to the first fragment, so the hit test looks there too.
 const f=this.getClientRects()[0]||r,x=f.x+f.width/2,y=f.y+f.height/2,top=root.elementFromPoint(x,y),hit=top===this||this.contains(top);
 // Scrolling into view does nothing when the box is already fully visible in
 // the viewport and in every scrolling ancestor.
 const html=document.documentElement,clips=e=>{const o=getComputedStyle(e);return o.overflowX!=='visible'||o.overflowY!=='visible';};
 let inView=r.left>=0&&r.top>=0&&r.right<=innerWidth&&r.bottom<=innerHeight,up=this,depth=0;
 while(inView&&(up=up.assignedSlot||up.parentElement||up.getRootNode().host)){
  if(++depth>64){inView=false;break;}
  // The viewport test covers the root, and a body whose overflow the viewport took over.
  if(up===html||(up===document.body&&!clips(html))||!clips(up))continue;
  const c=up.getBoundingClientRect(),l=c.left+up.clientLeft,t=c.top+up.clientTop;
  inView=r.left>=l&&r.top>=t&&r.right<=l+up.clientWidth&&r.bottom<=t+up.clientHeight;
 }
 // Painted at the click point without being the top element there: something covers it.
 const covered=!hit&&root.elementsFromPoint(x,y).some(e=>e===this||this.contains(e));
 // A click at that point would be taken by another control inside this element.
 let inner=false;for(let q=top;hit&&q&&q!==this;q=q.parentElement||q.getRootNode().host){if(marks.marked(q)||q.tagName==='LABEL'){inner=true;break;}}
 // An element that only looks like a control is named by the page: by the text it shows
 // outside the controls it holds, or else by a label in its markup.
 const words=()=>{const w=document.createTreeWalker(this,NodeFilter.SHOW_ELEMENT|NodeFilter.SHOW_TEXT,{acceptNode:n=>n.nodeType===1&&marks.marked(n)?NodeFilter.FILTER_REJECT:NodeFilter.FILTER_ACCEPT});let s='',n;
  for(let i=0;i<128&&s.length<=80&&(n=w.nextNode());i++){if(n.nodeType!==3)continue;const t=n.nodeValue.replace(/\s+/g,' ').trim(),p=n.parentElement;if(t&&p&&p.getClientRects().length&&getComputedStyle(p).visibility!=='hidden')s+=(s?' ':'')+t;}
  if(!s){const q=this.querySelector('img[alt],title');s=(attr(this,'aria-label')||attr(this,'title')||attr(this,'alt')||(q&&(q.getAttribute('alt')||q.textContent))||'').replace(/\s+/g,' ').trim();}
  return Array.from(s).slice(0,80).join('');};
 const readonly=(this.tagName==='INPUT'||this.tagName==='TEXTAREA')&&this.readOnly;
 const select=this.tagName==='SELECT'?{multiple:this.multiple,complete:this.options.length<=32,options:Array.from({length:Math.min(32,this.options.length)},(_,i)=>{const o=this.options[i];return {value:o.value.length<=256?o.value:null,label:o.label.slice(0,256),label_truncated:o.label.length>256,selected:o.selected,disabled:o.disabled||!!o.closest('optgroup')?.disabled};})}:null;
 if(select)select.complete=select.complete&&select.options.every(o=>o.value!==null&&!o.label_truncated);
 if(select)a.push(select);
 return {connected:true,signature:JSON.stringify(a),tag:this.tagName,type:this.type||'',editable:!readonly&&((this.tagName==='INPUT'&&!['button','submit','checkbox','radio','file','hidden'].includes(this.type))||this.tagName==='TEXTAREA'||this.isContentEditable),readonly,
 enabled:!this.disabled&&this.getAttribute('aria-disabled')!=='true',visible:r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none',hit,in_view:inView,covered,inner,x,y,checked:!!this.checked,select,steady,
 words:marks.native(this)||marks.role(this)?'':words()};}"#
);

pub(super) const SELECT: &str = r#"function(value){
 if(!this.isConnected||this.ownerDocument!==document||this.tagName!=='SELECT'||this.disabled||this.multiple||this.options.length>1024)return false;
 let chosen=null;for(const option of this.options){if(option.value===value){if(chosen||option.disabled||option.closest('optgroup')?.disabled)return false;chosen=option;}}
 if(!chosen)return false;
 Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(this,value);
 this.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
 this.dispatchEvent(new Event('change',{bubbles:true}));
 return true;
}"#;

// A date, a time, a colour and a slider hold a value that is picked, not typed.
// It is set the way a picker sets it. The field keeps only a value that it can
// hold, and a field of the same type and range that is not part of the page
// shows what that is. Without `apply` the function only says whether the value
// fits, and leaves the page as it was.
pub(super) const SET: &str = r#"function(value,apply){
 if(!this.isConnected||this.ownerDocument!==document||this.tagName!=='INPUT'||this.disabled)return false;
 const probe=document.createElement('input');probe.type=this.type;
 for(const name of ['min','max','step'])if(this.hasAttribute(name))probe.setAttribute(name,this.getAttribute(name));
 probe.value=value;
 // The browser writes a colour in lower case, and a number in its shortest form.
 const fits=this.type==='range'?value.trim()!==''&&Number(probe.value)===Number(value):probe.value.toLowerCase()===value.toLowerCase();
 if(!fits||!apply)return fits;
 const before=this.value;
 Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(this,value);
 if(this.value!==before){
  this.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
  this.dispatchEvent(new Event('change',{bubbles:true}));
 }
 return true;
}"#;

pub(super) const COLLECT: &str = include_str!("../observe.js");

pub(super) const FOCUSED: &str = r#"(()=>{
 if(!document.hasFocus())return null;
 let e=document.activeElement;while(e?.shadowRoot?.activeElement)e=e.shadowRoot.activeElement;
 return e&&((e instanceof HTMLInputElement&&!['button','submit','reset','checkbox','radio','file','hidden','range','color'].includes(e.type))||e instanceof HTMLTextAreaElement)&&!e.disabled&&!e.readOnly?e:null;
})()"#;

// This fixed isolated-world function edits the captured node, never whichever
// field happens to have focus after a navigation. It returns no field value.
pub(super) const INSERT: &str = r#"function(text){
 const focused=()=>{let e=document.activeElement;while(e?.shadowRoot?.activeElement)e=e.shadowRoot.activeElement;return e;};
 const valid=()=>this.isConnected&&this.ownerDocument===document&&document.hasFocus()&&focused()===this&&!this.disabled&&!this.readOnly;
 if(!valid())return 'stale';
 const proto=this instanceof HTMLInputElement?HTMLInputElement.prototype:this instanceof HTMLTextAreaElement?HTMLTextAreaElement.prototype:null;
 if(!proto)return 'stale';
 if(!this.dispatchEvent(new InputEvent('beforeinput',{bubbles:true,composed:true,cancelable:true,inputType:'insertText',data:text})))return 'prevented';
 if(!valid())return 'changed';
 const old=this.value,start=this.selectionStart??old.length,end=this.selectionEnd??old.length;
 Object.getOwnPropertyDescriptor(proto,'value').set.call(this,old.slice(0,start)+text+old.slice(end));
 try{this.setSelectionRange(start+text.length,start+text.length);}catch{}
 this.dispatchEvent(new InputEvent('input',{bubbles:true,composed:true,inputType:'insertText',data:text}));
 return 'inserted';
}"#;
