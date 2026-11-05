function() {
  // One bounded reader in this isolated world. A fresh observation closes the old one.
  globalThis.__rdhBrowserReader?.close();
  const interactive = new Set(['A', 'BUTTON', 'INPUT', 'TEXTAREA', 'SELECT', 'OPTION', 'SUMMARY']);
  const excluded = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD']);
  const filter = {acceptNode(node) {
    return node.nodeType === 1 && excluded.has(node.tagName) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT;
  }};
  const state = {pending: [], walkers: [document.createTreeWalker(document, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, filter)],
    text: null, changed: false, closed: false, omitted: new Set(), roots: []};
  const stopWatching=()=>{
    observer.disconnect();
    for(const root of [document,...state.roots]) {
      root.removeEventListener('input',invalidate,true);
      root.removeEventListener('change',invalidate,true);
    }
    state.roots=[];
  };
  const invalidate=()=>{state.changed=true;stopWatching();};
  const observer = new MutationObserver(invalidate);
  observer.observe(document, {subtree: true, childList: true, attributes: true, characterData: true});
  document.addEventListener('input',invalidate,true);document.addEventListener('change',invalidate,true);
  state.close = function() {stopWatching();this.pending=[];this.walkers=[];this.text=null;this.closed=true;};
  state.status = function() {return {changed:this.changed||this.closed,more:!!(this.pending.length||this.text||this.walkers.length)};};
  state.collect = function(maxNodes, maxScan) {
    let scanned=0;
    if(this.changed||this.closed)return {changed:true};
    if (!this.pending.length && (this.walkers.length||this.text)) {
      while ((this.walkers.length||this.text) && scanned<maxScan && this.pending.length<maxNodes) {
        if(this.text) {
          const {node,offset,preserve}=this.text, value=node.nodeValue;
          let end=Math.min(value.length,offset+512);
          if(end<value.length && value.charCodeAt(end-1)>=0xd800 && value.charCodeAt(end-1)<=0xdbff)end--;
          const name=value.slice(offset,end);
          this.text=end<value.length?{node,offset:end,preserve}:null;
          if(name.trim()||preserve)this.pending.push({role:'StaticText',name,text_continued:offset>0,text_remaining:!!this.text});
          continue;
        }
        const node=this.walkers[this.walkers.length-1].nextNode();
        if(!node){this.walkers.pop();continue;}
        scanned++;
        if(node.nodeType===3) {
          const parent=node.parentElement;
          if(!parent||parent.closest('button,a,select,textarea,script,style,h1,h2,h3,h4,h5,h6'))continue;
          const style=getComputedStyle(parent);
          if(style.display==='none'||style.visibility==='hidden'||!parent.getClientRects().length)continue;
          this.text={node,offset:0,preserve:node.nodeValue.length>512};continue;
        }
        if(node.shadowRoot) {
          if(this.walkers.length<64 && this.roots.length<128) {
            observer.observe(node.shadowRoot, {subtree:true,childList:true,attributes:true,characterData:true});
            node.shadowRoot.addEventListener('input',invalidate,true);
            node.shadowRoot.addEventListener('change',invalidate,true);
            this.roots.push(node.shadowRoot);
            this.walkers.push(document.createTreeWalker(node.shadowRoot,NodeFilter.SHOW_ELEMENT|NodeFilter.SHOW_TEXT,filter));
          }
          else this.omitted.add('shadow_scope_limit');
        } else if(node.tagName.includes('-'))this.omitted.add('custom_element_internals_unverified');
        if(!interactive.has(node.tagName)&&!/^H[1-6]$/.test(node.tagName)&&!node.hasAttribute('role')&&!node.isContentEditable)continue;
        let attributeBytes=0;
        for(const attribute of node.attributes){attributeBytes+=attribute.name.length+attribute.value.length;if(attributeBytes>8192)break;}
        if(attributeBytes>8192||node.childNodes.length>1024){this.omitted.add('oversized_control');continue;}
        this.pending.push({node});
      }
      // An empty batch can still have unvisited DOM nodes after the scan budget.
    }
    return {changed:false,scanned,omitted:[...this.omitted],nodes:this.pending.slice(0,maxNodes).map((entry,index)=>entry.node?{index}:{...entry})};
  };
  state.ack = function(count, chars) {
    this.pending.splice(0,count);
    if(chars>0&&this.pending[0]&&!this.pending[0].node){
      this.pending[0].name=Array.from(this.pending[0].name).slice(chars).join('');
      this.pending[0].text_continued=true;
    }
    return this.status();
  };
  globalThis.__rdhBrowserReader=state;
  return state;
}
