// Fixed semantic_dom_v1 program. Only adapters supply the closed operation and
// validated arguments. State lives in an isolated world / sandbox realm.
function(operation, args, other) {
  'use strict';
  const SLOT = '__rdhSemanticDomV1';
  const norm = value => String(value || '').replace(/[\t\n\f\r ]+/g, ' ').replace(/^ | $/g, '');
  const scalars = (value,limit=64001) => {const result=[];for(const scalar of value){if(result.length>=limit)break;result.push(scalar);}return result;};
  const prefix = (value,limit) => scalars(String(value),limit).join('').toWellFormed();
  // An ephemeral index belongs to this bounded script call, not the retained
  // node registry. Incomplete lookup can show a name but cannot issue a ref.
  const idRoots=new Map();
  const idsFor=root=>{if(idRoots.has(root))return idRoots.get(root);const ids=new Map(),walker=document.createTreeWalker(root,NodeFilter.SHOW_ELEMENT);let node,count=0;while((node=walker.nextNode())&&count<5000){count++;if(node.id){let entries=ids.get(node.id);if(!entries){entries=[];ids.set(node.id,entries);}if(entries.length<2)entries.push(node);}}const index={ids,complete:!node};idRoots.set(root,index);return index;};
  const forbidden = new Set(['generic','none','presentation','caption','code','deletion','emphasis','insertion','paragraph','strong','subscript','superscript']);
  const controls = new Set(['button','link','textbox','searchbox','combobox','checkbox','radio','spinbutton','slider','switch','menuitem','tab','option']);
  const contentNames = new Set(['button','link','checkbox','radio','switch','menuitem','tab','option','heading','inferred']);
  const picked = new Set(['date','time','datetime-local','month','week','color','range']);
  let state = globalThis[SLOT];
  if (!state || state.document !== document) {
    state = {document, realm:Array.from(crypto.getRandomValues(new Uint8Array(16)),byte=>byte.toString(16).padStart(2,'0')).join(''), restoration:0, serial:0, identities:new WeakMap(), nodes:new Map(), scan:null, revision:0,expires:0,timer:null};
    Object.defineProperty(globalThis, SLOT, {value:state, configurable:true});
    addEventListener('pageshow', event => {
      if (event.persisted) { state.restoration++; state.nodes.clear(); state.scan = null; }
    });
    state.observer = new MutationObserver(() => state.revision++);
    state.observer.observe(document, {subtree:true,childList:true,characterData:true,attributes:true});
  }
  if(state.expires&&performance.now()>=state.expires){state.nodes.clear();state.scan=null;state.expires=0;clearTimeout(state.timer);state.timer=null;}
  const retain=()=>{clearTimeout(state.timer);state.expires=performance.now()+300000;state.timer=setTimeout(()=>{state.nodes.clear();state.scan=null;state.expires=0;state.timer=null;},300000);};
  const identity = node => {
    let id = state.identities.get(node);
    if (!id) { id = ++state.serial; state.identities.set(node,id); }
    return id;
  };
  const parent = node => node.parentElement || node.getRootNode().host || null;
  const editingHost = node => node.isContentEditable && !parent(node)?.isContentEditable;
  const visible = node => {
    if (!node.isConnected || !node.getClientRects().length) return false;
    const style = getComputedStyle(node);
    return style.visibility !== 'hidden' && style.visibility !== 'collapse' && style.display !== 'none';
  };
  const disabled = node => node.matches(':disabled') || node.getAttribute('aria-disabled') === 'true' || !!node.closest('[inert]');
  const role = node => {
    const explicit = norm(node.getAttribute('role')).split(' ')[0];
    if (explicit && (controls.has(explicit) || forbidden.has(explicit) || explicit === 'heading' || explicit === 'img')) return {role:explicit,kind:'aria'};
    const tag = node.localName;
    let value = '';
    if (tag === 'button' || tag === 'summary') value = 'button';
    else if (tag === 'a' && node.hasAttribute('href')) value = 'link';
    else if (tag === 'textarea' || editingHost(node)) value = 'textbox';
    else if (tag === 'select') value = 'combobox';
    else if (tag === 'img') value = 'img';
    else if (/^h[1-6]$/.test(tag)) value = 'heading';
    else if (tag === 'input') {
      const type = node.type;
      if (type === 'hidden') return {role:'',kind:'native'};
      value = ['button','submit','reset','image'].includes(type) ? 'button' :
        ['checkbox','radio'].includes(type) ? type : type === 'range' ? 'slider' :
        type === 'number' ? 'spinbutton' : type === 'search' ? 'searchbox' : 'textbox';
    }
    if (value) return {role:value,kind:'native'};
    if ((!explicit || explicit === 'region') && scrollable(node)) return {role:explicit || 'generic',kind:'scrollable'};
    if (node.hasAttribute('tabindex') || node.hasAttribute('onclick') || getComputedStyle(node).cursor === 'pointer') return {role:'inferred',kind:'inferred'};
    return {role:explicit ? 'unsupported' : '',kind:explicit ? 'aria' : 'text'};
  };
  function nameOf(node, mapped) {
    const quality = new Set();
    const budget = {nodes:0,refs:0};
    const bounded=value=>{const text=String(value||'');if(text.length>8192)quality.add('name_character_limit');return norm(prefix(text,4097));};
    const text = root => {
      let result = '';
      const pending = [root];
      while (pending.length) {
        if (++budget.nodes > 256) { quality.add('name_node_limit'); break; }
        const item = pending.pop();
        if (item.nodeType === Node.TEXT_NODE) {if(item.nodeValue.length>8192)quality.add('name_character_limit');result += prefix(item.nodeValue,4097);}
        else if (item.nodeType === Node.ELEMENT_NODE) {
          if (['script','style','template','noscript'].includes(item.localName)) continue;
          if (item.localName === 'img' && item.hasAttribute('alt')) result += bounded(item.getAttribute('alt'));
          else if (item.localName === 'br') result += ' ';
          else for (let child = item.lastChild; child; child = child.previousSibling) {if(pending.length+budget.nodes>=256){quality.add('name_node_limit');break;}pending.push(child);}
        }
        if (result.length > 4096) { quality.add('name_character_limit'); break; }
      }
      return norm(result);
    };
    const associated = (element, path, depth) => {
      if (depth > 16) { quality.add('name_depth_limit'); return ''; }
      if (path.has(element)) { quality.add('cyclic_reference'); return ''; }
      const ids = bounded(element.getAttribute('aria-labelledby'));
      if (!ids) return text(element);
      const next = new Set(path); next.add(element);
      const pieces = [];
      for (const id of ids.split(' ')) {
        if (++budget.refs > 32) { quality.add('name_reference_limit'); break; }
        const root = element.getRootNode();
        const index=idsFor(root);if(!index.complete)quality.add('name_lookup_limit');
        const matches = index.ids.get(id)||[];
        if (!matches.length) { quality.add('missing_reference'); continue; }
        if (matches.length !== 1) { quality.add('ambiguous_reference'); continue; }
        const value = associated(matches[0], next, depth + 1);
        if (value) pieces.push(value);
        else if (!quality.has('cyclic_reference') && !quality.has('name_depth_limit')) quality.add('empty_reference');
      }
      return norm(pieces.join(' '));
    };
    // A scroll handle uses the container label even when the DOM role prohibits an AX name.
    if (forbidden.has(mapped.role) && mapped.kind !== 'scrollable') return {name:'',source:'prohibited',quality:[]};
    if (mapped.role === 'unsupported') quality.add('unsupported_role');
    let name = '', source = 'none';
    if (bounded(node.getAttribute('aria-labelledby'))) { name = associated(node,new Set(),0); if (name) source = 'aria-labelledby'; }
    if (!name && bounded(node.getAttribute('aria-label'))) { name = bounded(node.getAttribute('aria-label')); source = 'aria-label'; }
    if (!name && ['input','textarea','select','button','meter','progress','output'].includes(node.localName) && node.labels?.length) {
      const labels = [];
      for (const label of node.labels) {
        if (++budget.refs > 32) { quality.add('name_reference_limit'); break; }
        if (label.getRootNode() !== node.getRootNode()) { quality.add('missing_reference'); continue; }
        const value = text(label);
        if (value) labels.push(value); else quality.add('empty_reference');
      }
      name = norm(labels.join(' ')); if (name) source = 'label';
    }
    if (!name && (node.localName === 'img' || (node.localName === 'input' && node.type === 'image'))) { name = bounded(node.getAttribute('alt')); if (name) source = 'alt'; }
    if (!name && node.localName === 'input' && ['button','submit','reset'].includes(node.type) && node.hasAttribute('value')) { name = bounded(node.value); if (name) source = 'value'; }
    if (!name && contentNames.has(mapped.role)) { name = text(node); if (name) source = 'content'; }
    if (!name) { name = bounded(node.getAttribute('title')); if (name) source = 'title'; }
    if (scalars(name).length > 512) { name = scalars(name).slice(0,512).join(''); quality.add('name_character_limit'); }
    return {name,source,quality:Array.from(quality).sort()};
  }
  const sensitive = node => node.localName === 'input' && (['password','file'].includes(node.type) || /(?:pass(?:word|code)?|secret|token|one-time-code|cc-number|cc-csc)/i.test([node.name,node.id,node.autocomplete].join(' ')));
  // Browsers retain a sole BR as the caret placeholder of an empty editing
  // host. It contributes no character to the editing value.
  const editableValue = node => !node.firstChild || node.childNodes.length===1 && node.firstChild.nodeName==='BR' ? '' : node.innerText.replace(/\r\n/g,'\n');
  const valueOf = node => sensitive(node) ? null : typeof node.value === 'string' ? node.value : editingHost(node) ? editableValue(node) : null;
  const semanticAttributes = ['id','name','role','type','href','target','action','formaction','method','aria-label','aria-labelledby','aria-checked','aria-disabled','aria-readonly','aria-expanded','aria-selected','aria-pressed','alt','title','for','disabled','readonly','multiple','min','max','step','tabindex','contenteditable','data-rdh-record'];
  function material(node) {
    let boundary = node;
    for (let current = node; current; current = parent(current)) {
      if (current.matches('[data-rdh-record],tr,li,article,[role="row"],[role="listitem"]')) { boundary = current; break; }
    }
    const pending = [boundary], entries = [], limits = new Set();
    let characters = 0,visited=0;
    const append=child=>{if(pending.length+visited>=256){limits.add('record_node_limit');return false;}pending.push(child);return true;};
    while (pending.length) {
      if (++visited > 256) { limits.add('record_node_limit'); break; }
      const item = pending.pop();
      if (item.nodeType === Node.TEXT_NODE) {
        if(item.nodeValue.length>8192){limits.add('record_character_limit');break;}
        const text = norm(prefix(item.nodeValue,4097));
        if (text) { characters += scalars(text).length; entries.push({identity:identity(item),tag:'#text',attributes:[],text,value:null,checked:false}); }
      } else if (item.nodeType === Node.ELEMENT_NODE) {
        if (['script','style','template','noscript'].includes(item.localName)) continue;
        const attributes = semanticAttributes.filter(key => item.hasAttribute(key)).map(key => [key,prefix(item.getAttribute(key),4097)]);
        const value = valueOf(item);
        characters += attributes.reduce((sum,pair) => sum+scalars(pair[1]).length,0) + scalars(value || '',4097).length;
        entries.push({identity:identity(item),tag:item.localName,attributes,text:'',value,checked:!!item.checked});
        if (item.shadowRoot) for (let child = item.shadowRoot.lastChild; child; child = child.previousSibling) if(!append(child))break;
        for (let child = item.lastChild; child; child = child.previousSibling) if(!append(child))break;
      }
      if (characters > 4096) { limits.add('record_character_limit'); entries.pop(); break; }
    }
    return {entries,complete:limits.size === 0,limits:Array.from(limits).sort()};
  }
  const scrollable = node => {
    if (!node || node.nodeType !== 1 || !node.isConnected || node.ownerDocument !== document) return false;
    const style = getComputedStyle(node);
    const vertical = style.overflowY === "auto" || style.overflowY === "scroll";
    const named = node.getAttribute("role") === "region" || node.hasAttribute("aria-label") || node.hasAttribute("aria-labelledby") || node.tabIndex >= 0;
    return vertical && node.scrollHeight > node.clientHeight && named && visible(node);
  };
  function facts(node) {
    if (!node || !node.isConnected || node.ownerDocument !== document) return null;
    const mapped = role(node), name = nameOf(node,mapped), box = node.getBoundingClientRect();
    const x = (Math.max(0,box.left)+Math.min(innerWidth,box.right))/2;
    const y = (Math.max(0,box.top)+Math.min(innerHeight,box.bottom))/2;
    const root = node.getRootNode();
    const top = root.elementFromPoint ? root.elementFromPoint(x,y) : document.elementFromPoint(x,y);
    let hit = top === node || !!top && node.contains(top);
    for (let inner = top; hit && inner && inner !== node; inner = parent(inner)) {
      const childRole = role(inner);
      if (controls.has(childRole.role) || inner.localName === 'label') hit = false;
    }
    const record = material(node);
    const value = valueOf(node);
    if (value !== null && (value.length>8192 || scalars(value).length > 4096)) { record.complete = false; record.limits.push('value_character_limit'); }
    return {node:identity(node),id:node.id || '',role:mapped.role,kind:mapped.kind,
      name:name.name,name_source:name.source,name_quality:name.quality,
      value:value === null ? null : prefix(value,4096),
      enabled:!disabled(node),readonly:!!node.readOnly || node.getAttribute('aria-readonly') === 'true',
      editable:editingHost(node) || node.localName === 'textarea' || node.localName === 'input' && !['button','submit','reset','checkbox','radio','file','hidden','image'].includes(node.type),
      sensitive:sensitive(node),checked:!!node.checked || node.getAttribute('aria-checked') === 'true',
      tag:node.localName,input_type:node.type || '',visible:visible(node),hit,
      x:Number.isFinite(x) ? x : 0,y:Number.isFinite(y) ? y : 0,
      in_view:box.left >= 0 && box.top >= 0 && box.right <= innerWidth && box.bottom <= innerHeight,
      record};
  }
  const target = () => {
    if (args.realm !== state.realm || args.restoration !== state.restoration) return null;
    return state.nodes.get(args.node) || null;
  };
  const stamp = () => ({realm:state.realm,restoration:state.restoration,url:location.href});
  const push = (pending,node,omitted) => {
    if (pending.length>=50000) { omitted.add('traversal_limit'); return false; }
    pending.push(node); return true;
  };
  const result = value => JSON.stringify(value);
  if (operation === 'stamp') return result(stamp());
  if (operation === 'ready') return result(document.readyState === 'complete');
  if (operation === 'stats') return result({nodes:state.nodes.size,scans:state.scan?1:0,pending:state.scan?.pending.length||0});
  if (operation === 'clear_nodes') {if(args.realm!==state.realm||args.restoration!==state.restoration)return result(false);state.nodes.clear();return result(true);}
  if (operation === 'open') {
    retain();
    state.nodes.clear();
    state.scan = {pending:document.documentElement ? [document.documentElement] : [],query:args,revision:state.revision,sequence:0,scanned:0};
    return result(stamp());
  }
  if (operation === 'next') {
    const scan = state.scan;
    if (!scan || args.realm !== state.realm || args.restoration !== state.restoration || args.sequence !== scan.sequence) return result(null);
    retain();
    state.nodes.clear();
    const rows = [], omitted = new Set(['closed_shadow_not_observable','canvas_pixels_outside_profile','browser_ui_outside_profile']);
    let count = 0, characters = 0;
    while (scan.pending.length && rows.length < args.nodes && count < 5000) {
      const node = scan.pending.pop(); count++; scan.scanned++;
      if (node.nodeType === Node.TEXT_NODE) {
        if (scan.query.filter==='interactive' || !norm(node.nodeValue) || !node.parentElement || !visible(node.parentElement)) continue;
        let inside=false;
        for (let p=node.parentElement;p;p=parent(p)) {const mapped=role(p);if(controls.has(mapped.role)||contentNames.has(mapped.role)){inside=true;break;}}
        if (inside) continue;
        const owner=node.parentElement,box=owner.getBoundingClientRect();
        if (scan.query.filter==='viewport' && !(box.bottom>0&&box.right>0&&box.top<innerHeight&&box.left<innerWidth)) continue;
        const text=norm(prefix(node.nodeValue,513));
        if (scan.query.find && !text.includes(scan.query.find)) continue;
        const row=facts(owner);
        row.node=identity(node);row.id='';row.role='text';row.kind='text';row.name=scalars(text).slice(0,512).join('');row.name_source='content';row.name_quality=[];row.value=null;
        if(node.nodeValue.length>1024 || scalars(text).length>512){row.name_quality.push('text_character_limit');omitted.add('text_character_limit');}
        const length=scalars(JSON.stringify(row)).length;
        if(characters+length>args.characters){scan.pending.push(node);if(!rows.length){scan.pending.pop();omitted.add('row_character_limit');}break;}
        rows.push(row);characters+=length;continue;
      }
      if (node.nodeType !== Node.ELEMENT_NODE) continue;
      if (['script','style','template','noscript'].includes(node.localName)) continue;
      const mapped = role(node);
      const inViewport = () => { const box=node.getBoundingClientRect(); return box.bottom>0 && box.right>0 && box.top<innerHeight && box.left<innerWidth; };
      if (mapped.role && visible(node) && (scan.query.filter!=='interactive'||controls.has(mapped.role)||mapped.role==='heading'||mapped.role==='inferred'||mapped.kind==='scrollable') && (scan.query.filter !== 'viewport' || inViewport())) {
        const row = facts(node);
        const search=row.record.entries.map(entry=>[entry.text,entry.value||'',...entry.attributes.filter(pair=>['aria-label','alt','title'].includes(pair[0])).map(pair=>pair[1])].join(' ')).join(' ');
        const match = !scan.query.find || norm(search).includes(scan.query.find) || row.name.includes(scan.query.find);
        if (match) {
          const length = scalars(JSON.stringify(row)).length;
          if (characters+length > args.characters) { scan.pending.push(node); if (!rows.length) { scan.pending.pop(); omitted.add('row_character_limit'); } break; }
          rows.push(row); state.nodes.set(row.node,node); characters += length;
        }
      }
      if (node.shadowRoot) {
        state.observer.observe(node.shadowRoot,{subtree:true,childList:true,characterData:true,attributes:true});
        for (let child = node.shadowRoot.lastChild; child; child = child.previousSibling) if(!push(scan.pending,child,omitted))break;
      }
      for (let child = node.lastChild; child; child = child.previousSibling) if(!push(scan.pending,child,omitted))break;
    }
    scan.sequence++;
    if(!scan.pending.length)state.scan=null;
    return result({stamp:stamp(),rows,sequence:scan.sequence,more:scan.pending.length>0,scanned:count,
      changed:scan.revision !== state.revision,omitted:Array.from(omitted).sort(),characters});
  }
  const activeElement=()=>{let node=document.activeElement;while(node?.shadowRoot?.activeElement)node=node.shadowRoot.activeElement;return node;};
  const viewportHit=(x,y)=>{
    let node=document.elementFromPoint(x,y);
    for(let depth=0;node?.shadowRoot&&depth<64;depth++){
      const child=node.shadowRoot.elementFromPoint(x,y);if(!child||child===node)break;node=child;
    }
    return node;
  };
  if(operation==='frame_at_point')return result(['iframe','frame'].includes(viewportHit(args.x,args.y)?.localName));
  if(operation==='native_frame_point'){
    const node=other;
    if(!node||node.ownerDocument!==document||!node.isConnected||viewportHit(args.x,args.y)!==node||!['iframe','frame'].includes(node.localName))return result(null);
    // Human coordinates originate in the top-level screenshot. Only a proven
    // positive axis-aligned content-box mapping may descend into a child.
    for(let e=node;e;e=parent(e)){const s=getComputedStyle(e);if(s.transform!=='none'){const m=new DOMMatrixReadOnly(s.transform);if(!m.is2D||m.b!==0||m.c!==0||m.a<=0||m.d<=0)return result(null);}}
    if(!node.offsetWidth||!node.offsetHeight||!node.clientWidth||!node.clientHeight||!visible(node))return result(null);
    const r=node.getBoundingClientRect(),sx=r.width/node.offsetWidth,sy=r.height/node.offsetHeight;
    let left=r.left+node.clientLeft*sx,top=r.top+node.clientTop*sy,width=node.clientWidth*sx,height=node.clientHeight*sy;
    if(typeof node.getBoxQuads==='function'){
      const quads=node.getBoxQuads({box:'content'});if(quads.length!==1)return result(null);const q=quads[0];
      if(Math.abs(q.p1.y-q.p2.y)>0.001||Math.abs(q.p2.x-q.p3.x)>0.001||Math.abs(q.p3.y-q.p4.y)>0.001||Math.abs(q.p4.x-q.p1.x)>0.001)return result(null);
      left=q.p1.x;top=q.p1.y;width=q.p2.x-q.p1.x;height=q.p4.y-q.p1.y;
    }
    if(width<=0||height<=0)return result(null);
    const x=(args.x-left)/width*args.width,y=(args.y-top)/height*args.height;
    if(!Number.isFinite(x)||!Number.isFinite(y)||x<0||y<0||x>=args.width||y>=args.height)return result(null);
    return result({x,y,width:args.width,height:args.height,visible:true,hit:true,in_view:true});
  }
  if(operation==='native_focused_frame')return result(!!other&&other.isConnected&&other.ownerDocument===document&&activeElement()===other);
  if(operation==='scroll_viewport'){
    if(!Number.isFinite(args.delta)||Math.abs(args.delta)>2000)return result(false);
    scrollBy({top:args.delta,left:0,behavior:'instant'});return result(true);
  }
  if(operation==='surface') {
    let node=activeElement(),focus=null;
    // Headless Firefox can keep hasFocus() false after native pointer input.
    // This is a candidate; adapters separately prove every focused iframe owner.
    if(node&&node.isConnected&&!disabled(node)&&!node.readOnly&&visible(node)&&
       (node.localName==='textarea'||node.localName==='input'&&['text','search','url','tel','email','password'].includes(node.type))&&node.value.length<=16384) {
      focus={node:identity(node),tag:node.localName,input_type:node.type||'',value:node.value,start:node.selectionStart,end:node.selectionEnd};
    }
    return result({title:prefix(document.title,4096),url:location.href,width:innerWidth,height:innerHeight,scroll_x:scrollX,scroll_y:scrollY,focus});
  }
  if (operation === 'inspect') return result(facts(target()));
  if (operation === 'geometry') {
    const f=facts(target());return result(f?{x:f.x,y:f.y,width:innerWidth,height:innerHeight,visible:f.visible,hit:f.hit,in_view:f.in_view}:null);
  }
  if(operation==='native_frame'){
    const node=other;if(!node||!node.isConnected||node.ownerDocument!==document||!['iframe','frame'].includes(node.localName))return result(null);
    const r=node.getBoundingClientRect();let q;
    if(typeof node.getBoxQuads==='function') {
      const quads=node.getBoxQuads({box:'content'});if(quads.length!==1)return result(null);q=quads[0];
    } else {
      // A bounding rectangle proves only a positive, axis-aligned mapping.
      // Refuse rotations/perspective instead of guessing a clickable point.
      for(let e=node;e;e=parent(e)){const s=getComputedStyle(e);if(s.transform!=='none'){const m=new DOMMatrixReadOnly(s.transform);if(!m.is2D||m.b!==0||m.c!==0||m.a<=0||m.d<=0)return result(null);}}
      if(!node.offsetWidth||!node.offsetHeight)return result(null);
      const sx=r.width/node.offsetWidth,sy=r.height/node.offsetHeight;
      const x=r.left+node.clientLeft*sx,y=r.top+node.clientTop*sy,w=node.clientWidth*sx,h=node.clientHeight*sy;
      q={p1:{x,y},p2:{x:x+w,y},p3:{x:x+w,y:y+h},p4:{x,y:y+h}};
    }
    const u=args.x/args.width,v=args.y/args.height;
    const x=q.p1.x*(1-u)*(1-v)+q.p2.x*u*(1-v)+q.p3.x*u*v+q.p4.x*(1-u)*v;
    const y=q.p1.y*(1-u)*(1-v)+q.p2.y*u*(1-v)+q.p3.y*u*v+q.p4.y*(1-u)*v;
    const root=node.getRootNode(),top=root.elementFromPoint?root.elementFromPoint(x,y):document.elementFromPoint(x,y);
    return result({x,y,width:innerWidth,height:innerHeight,visible:args.visible&&visible(node),hit:args.hit&&top===node,in_view:args.in_view&&x>=0&&y>=0&&x<innerWidth&&y<innerHeight});
  }
  if(operation==='native_hit'){
    const node=target();if(!node||!node.isConnected)return result(false);
    for(let item=other,depth=0;item&&depth<64;item=parent(item),depth++){
      if(item===node)return result(true);
      if(controls.has(role(item).role)||item.localName==='label')return result(false);
    }
    return result(false);
  }
  if (operation === 'recheck') {
    if(args.realm!==state.realm||args.restoration!==state.restoration)return result(args.nodes.map(()=>null));
    return result(args.nodes.map(node=>facts(state.nodes.get(node))));
  }
  if (operation === 'scroll_target') {
    const node = target();
    if (!scrollable(node) || disabled(node) || !Number.isFinite(args.delta) || Math.abs(args.delta) > 2000) return result(null);
    const before = node.scrollTop;
    node.scrollBy({top:args.delta,behavior:'instant'});
    return result(node.scrollTop !== before);
  }
  if (operation === 'scroll') {
    const node=target(); if (!node || !node.isConnected) return result(false);
    node.scrollIntoView({block:'center',inline:'center',behavior:'instant'}); return result(true);
  }
  if (operation === 'focus') {
    const node=target(); if (!node || !node.isConnected || disabled(node)) return result(false);
    node.focus({preventScroll:true});
    // Focus alone does not put Firefox's document selection inside an editing
    // host. Position a caret in the retained node before native editing keys.
    if(editingHost(node)){const selection=getSelection();if(!node.contains(selection.anchorNode)||!node.contains(selection.focusNode)){const range=document.createRange();range.selectNodeContents(node);range.collapse(false);selection.removeAllRanges();selection.addRange(range);}}
    let active=document.activeElement;
    while (active?.shadowRoot?.activeElement) active=active.shadowRoot.activeElement;
    return result(active===node);
  }
  if (operation === 'form' || operation === 'form_check') {
    const node=target(); if (!node || !node.isConnected || disabled(node) || node.readOnly || sensitive(node)) return result(null);
    const before=node.value;
    if (node.localName === 'select') {
      if (node.multiple || node.options.length>1024) return result(null);
      const matches=Array.from(node.options).filter(option=>option.value===args.value);
      if (matches.length!==1 || matches[0].disabled || matches[0].closest('optgroup')?.disabled) return result(null);
      if (operation === 'form_check') return result(true);
      if (before===args.value) return result(false);
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(node,args.value);
    } else if (node.localName === 'input' && picked.has(node.type)) {
      const probe=document.createElement('input'); probe.type=node.type;
      for (const key of ['min','max','step']) if (node.hasAttribute(key)) probe.setAttribute(key,node.getAttribute(key));
      probe.value=args.value;
      const fits=node.type==='range' ? args.value.trim()!=='' && Number(probe.value)===Number(args.value) : probe.value.toLowerCase()===args.value.toLowerCase();
      if (!fits) return result(null);
      if (operation === 'form_check') return result(true);
      if (before===probe.value) return result(false);
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(node,args.value);
    } else return result(null);
    const expected=node.value;
    node.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
    node.dispatchEvent(new Event('change',{bubbles:true}));
    return result(node.isConnected&&node.value===expected?true:null);
  }
  if (operation === 'release') { state.nodes.clear(); state.scan=null;clearTimeout(state.timer);state.timer=null;state.expires=0;return result(true); }
  throw new Error('Unknown fixed semantic operation');
}
