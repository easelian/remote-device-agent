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
    // The evidence program in this isolated realm uses the same boundaries for record waits.
    // A fallback-only record role is opaque: never attribute its text to an outer record.
    const recordRoles = new Set(['row','listitem','article','option']);
    state.recordKind = node => {
      if (node.matches('[data-rdh-record],tr,li,article')) return 'record';
      const role = node.getAttribute('role');
      if (!role) return null;
      if (role.length > 4096) return 'ambiguous';
      const tokens = role.toLowerCase().match(/[^\t\n\f\r ]+/g) || [];
      if (recordRoles.has(tokens[0])) return 'record';
      return tokens.some(token => recordRoles.has(token)) ? 'ambiguous' : null;
    };
    state.sensitiveControl = (node, onAttributeLimit) => {
      const fields=[node.getAttribute('name'),node.getAttribute('id'),node.getAttribute('autocomplete')];
      // An oversized sensitivity hint cannot be declared non-sensitive after
      // truncating away the suffix that identifies it.
      if(fields.some(value=>value && value.length>256)) { onAttributeLimit?.(); return true; }
      return ['password','file'].includes(node.type) || /(?:pass(?:word|code)?|secret|token|one-time-code|cc-number|cc-csc)/i.test(fields.join(' '));
    };
    state.nativeValidation = (node, onAttributeLimit) => {
      if (!(node instanceof HTMLInputElement || node instanceof HTMLSelectElement || node instanceof HTMLTextAreaElement)) {
        return {state:'not_applicable',reasons:[]};
      }
      if (state.sensitiveControl(node, onAttributeLimit)) return {state:'omitted',reasons:[]};
      if (!node.willValidate) return {state:'not_applicable',reasons:[]};
      // Reading validity flags does not dispatch an invalid event or reveal the site-provided message.
      const validity = node.validity;
      if (validity.valid) return {state:'valid',reasons:[]};
      const flags = [
        ['valueMissing','value_missing'], ['typeMismatch','type_mismatch'],
        ['patternMismatch','pattern_mismatch'], ['tooLong','too_long'], ['tooShort','too_short'],
        ['rangeUnderflow','range_underflow'], ['rangeOverflow','range_overflow'],
        ['stepMismatch','step_mismatch'], ['badInput','bad_input'], ['customError','custom_error']
      ];
      const reasons = flags.filter(([flag]) => validity[flag]).map(([,reason]) => reason);
      return {state:reasons.length ? 'invalid' : 'omitted',reasons};
    };
    state.clearNodes = () => {
      state.reuse?.observer.disconnect();
      state.reuse = null;
      state.nodes.clear();
    };
    Object.defineProperty(globalThis, SLOT, {value:state, configurable:true});
    addEventListener('pageshow', event => {
      if (event.persisted) { state.restoration++; state.clearNodes(); state.scan = null; }
    });
    state.observer = new MutationObserver(() => state.revision++);
    state.observer.observe(document, {subtree:true,childList:true,characterData:true,attributes:true});
  }
  if(state.expires&&performance.now()>=state.expires){state.clearNodes();state.scan=null;state.expires=0;clearTimeout(state.timer);state.timer=null;}
  const retain=()=>{clearTimeout(state.timer);state.expires=performance.now()+300000;state.timer=setTimeout(()=>{state.clearNodes();state.scan=null;state.expires=0;state.timer=null;},300000);};
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
  const explicitRole = node => norm(node.getAttribute('role')).split(' ')[0];
  const role = node => {
    const explicit = explicitRole(node);
    // A virtual listbox is a scroll target when it has real vertical overflow.
    // Keep its ARIA role so the client can distinguish the container from a
    // generic region. Its options remain separately observable controls.
    if (explicit === 'listbox' && scrollable(node)) return {role:'listbox',kind:'scrollable'};
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
  function optionPosition(item) {
    const result = [];
    for (const key of ['aria-posinset','aria-setsize']) {
      const raw = item.getAttribute(key);
      if (raw === null) continue;
      if (key === 'aria-setsize' && raw === '-1') {
        result.push([key, raw]);
      } else {
        if (raw.length > 10 || !/^[0-9]+$/.test(raw)) return null;
        const value = Number(raw);
        if (!value || value > 2147483647) return null;
        result.push([key, String(value)]);
      }
    }
    if (result.length === 2 && result[1][1] !== '-1' && Number(result[0][1]) > Number(result[1][1])) return null;
    return result;
  }
  function material(node, mapped) {
    // Scrolling needs the container identity, not the content of every mounted
    // option. All other refs keep their enclosing business record in the fingerprint.
    const onlyContainer = mapped.kind === 'scrollable' && mapped.role === 'listbox';
    const scopes = [], entries = [], ownerContext = [], limits = new Set();
    if (!onlyContainer) {
      let depth = 0;
      for (let current = node; current; current = parent(current)) {
        if (++depth > 64) { limits.add('record_depth_limit'); break; }
        const kind = state.recordKind(current);
        if (!kind) continue;
        if (!scopes.length && kind === 'ambiguous' && current !== node) scopes.push(node);
        scopes.push(current);
      }
    }
    if (!scopes.length) scopes.push(node);
    let ownerList;
    if (mapped.role === 'option') {
      let depth = 0;
      for (let owner = parent(node); owner; owner = parent(owner)) {
        if (++depth > 64) { limits.add('record_depth_limit'); break; }
        if (explicitRole(owner) === 'listbox') { ownerList = identity(owner); break; }
      }
    }
    let characters = 0, visited = 0;
    // Keep enclosing records in the private action fingerprint. Their contents
    // must never make a child control match a public search, or vice versa.
    for (const scope of scopes) {
      if (characters > 4096) break;
      if (visited >= 256) { limits.add('record_node_limit'); break; }
      const pending = [scope];
      const destination = scope === scopes[0] ? entries : ownerContext;
      const appendChildren = root => {
        for (let child = root.lastChild; child; child = child.previousSibling) {
          if (pending.length + visited >= 256) {
            limits.add('record_node_limit');
            break;
          }
          pending.push(child);
        }
      };
      while (pending.length) {
        if (++visited > 256) { limits.add('record_node_limit'); break; }
        const item = pending.pop();
        if (item.nodeType === Node.TEXT_NODE) {
          if(item.nodeValue.length>8192){limits.add('record_character_limit');break;}
          const text = norm(prefix(item.nodeValue,4097));
          if (text) { characters += scalars(text).length; destination.push({identity:identity(item),tag:'#text',attributes:[],text,value:null,checked:false}); }
        } else if (item.nodeType === Node.ELEMENT_NODE) {
          if (item !== scope && state.recordKind(item)) continue;
          if (['script','style','template','noscript'].includes(item.localName)) continue;
          const attributes = semanticAttributes.filter(key => item.hasAttribute(key)).map(key => [key,prefix(item.getAttribute(key),4097)]);
          if (explicitRole(item) === 'option') {
            const position = optionPosition(item);
            if (position) attributes.push(...position);
            else if (item === node) limits.add('invalid_option_position');
          }
          const value = valueOf(item);
          characters += attributes.reduce((sum,pair) => sum+scalars(pair[1]).length,0) + scalars(value || '',4097).length;
          destination.push({identity:identity(item),tag:item.localName,attributes,text:'',value,checked:!!item.checked});
          if (!onlyContainer) {
            if (item.shadowRoot) appendChildren(item.shadowRoot);
            appendChildren(item);
          }
        }
        if (characters > 4096) { limits.add('record_character_limit'); destination.pop(); break; }
      }
    }
    return {entries, owner_context:ownerContext.length ? ownerContext : undefined, owner_list:ownerList,
      complete:limits.size === 0, limits:Array.from(limits).sort()};
  }
  const scrollable = node => {
    if (!node || node.nodeType !== 1 || !node.isConnected || node.ownerDocument !== document) return false;
    const style = getComputedStyle(node);
    const vertical = style.overflowY === "auto" || style.overflowY === "scroll";
    const named = node.getAttribute("role") === "region" || node.hasAttribute("aria-label") || node.hasAttribute("aria-labelledby") || node.tabIndex >= 0;
    return vertical && node.scrollHeight > node.clientHeight && named && visible(node);
  };
  const dropRoles = new Set(['region','list','listbox','group']);
  function endpointMarker(node) {
    if (node.draggable && node.getAttribute('draggable')?.toLowerCase() === 'true') return 'drag_source';
    const effect = norm(node.getAttribute('aria-dropeffect')).toLowerCase();
    return dropRoles.has(explicitRole(node)) || node.hasAttribute('ondrop') || effect && effect !== 'none' ? 'drop_target' : '';
  }
  function endpointFacts(node) {
    if (!node || !node.isConnected || node.ownerDocument !== document) return null;
    const kind = endpointMarker(node);
    if (!kind || !visible(node) || ['input','textarea','select'].includes(node.localName) || node.isContentEditable) return null;
    let current = node;
    for (let depth = 0; current && depth < 64; depth++, current = parent(current)) {
      if (disabled(current) || sensitive(current) || current.isContentEditable) return null;
    }
    if (current) return null;
    const mapped = role(node);
    const explicit = explicitRole(node);
    if (dropRoles.has(explicit)) mapped.role = explicit;
    if (!mapped.role) mapped.role = 'group';
    const row = facts(node, mapped, kind);
    return row.name && !row.name_quality.length ? row : null;
  }
  function facts(node, mapped, endpoint) {
    if (!node || !node.isConnected || node.ownerDocument !== document) return null;
    mapped = mapped || role(node);
    const name = nameOf(node,mapped), box = node.getBoundingClientRect();
    const x = (Math.max(0,box.left)+Math.min(innerWidth,box.right))/2;
    const y = (Math.max(0,box.top)+Math.min(innerHeight,box.bottom))/2;
    const root = node.getRootNode();
    const top = root.elementFromPoint ? root.elementFromPoint(x,y) : document.elementFromPoint(x,y);
    let hit = top === node || !!top && node.contains(top);
    for (let inner = top; hit && inner && inner !== node; inner = parent(inner)) {
      const childRole = role(inner);
      if (controls.has(childRole.role) || inner.localName === 'label' || endpoint && endpointMarker(inner)) hit = false;
    }
    const record = material(node,mapped);
    const value = valueOf(node);
    if (value !== null && (value.length>8192 || scalars(value).length > 4096)) { record.complete = false; record.limits.push('value_character_limit'); }
    return {node:identity(node),id:node.id || '',role:mapped.role,kind:endpoint || mapped.kind,
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
  // The proof stays in the isolated realm. Reject any DOM mutation in the
  // document or its same-origin embedding chain. Native radio checkedness is
  // a property change, so verify the complete group separately before use.
  function rememberSubmitPairs(rows) {
    if (window === window.top || rows.length > 200) return;
    const radios = rows.filter(row => row.kind === 'native' && row.role === 'radio' && !row.checked);
    if (!radios.length || radios.length > 32) return;
    const pairs = new Map(), forms = new Map(), observed = new Set(rows.map(row => row.node));
    const recordOf = node => {
      for (let depth = 0; node && depth < 64; depth++, node = node.parentElement) {
        const kind = state.recordKind(node);
        if (kind) return kind === 'record' ? node : undefined;
      }
      return node ? undefined : null;
    };
    for (const row of radios) {
      const radio = state.nodes.get(row.node), form = radio.form;
      if (!form || !radio.name || radio.getRootNode() !== document || !form.contains(radio)) continue;
      if (!forms.has(form)) {
        forms.set(form, null);
        const controls = form.elements;
        if (!controls || !Number.isInteger(controls.length) || controls.length > 64) continue;
        const members = Array.from(controls);
        if (members.some(n => !form.contains(n) || sensitive(n) ||
            !(n.localName === 'fieldset' || n.localName === 'button' && n.type === 'submit' ||
              n.localName === 'input' && ['radio','hidden','submit'].includes(n.type)))) continue;
        // image inputs are excluded from form.elements; decline the optional shortcut.
        if (document.querySelector('input[type="image" i]')) continue;
        const buttons = members.filter(n => n.type === 'submit');
        if (buttons.length !== 1 || !observed.has(identity(buttons[0]))) continue;
        const submit = buttons[0], owner = recordOf(submit);
        if (owner === undefined || submit.getRootNode() !== document) continue;
        if (!owner) {
          // Standalone iframe editor: require the form's hidden record key to
          // also be visible in its document heading. A matching caption alone
          // cannot bind controls from different forms.
          if (document.forms.length !== 1) continue;
          const headings = document.getElementsByTagName('h1');
          if (headings.length !== 1 || !visible(headings[0])) continue;
          const caption = headings[0].innerText;
          if (!caption || caption.length > 512 || !members.some(n => n.type === 'hidden' &&
              n.name && n.value.length >= 1 && n.value.length <= 128 &&
              caption.split(/[^\p{L}\p{N}_-]+/u).includes(n.value))) continue;
        }
        const group = members.filter(n => n.type === 'radio').map(n => [n, n.checked]);
        forms.set(form, {form, submit, owner, radios:group});
      }
      const proof = forms.get(form);
      if (proof && recordOf(radio) === proof.owner) pairs.set(radio, proof);
    }
    if (!pairs.size) return;
    const documents = [], observer = new MutationObserver(() => { guard.invalid = true; });
    const guard = {documents, observer, pairs, invalid:false};
    try {
      let win = window;
      for (let depth = 0; depth <= 8; depth++) {
        const doc = win.document;
        documents.push([doc, doc.URL]);
        observer.observe(doc, {subtree:true, attributes:true, characterData:true, childList:true});
        if (win === win.top) { state.reuse = guard; return; }
        const embed = win.frameElement;
        if (!embed || !embed.isConnected || embed.getRootNode() !== embed.ownerDocument) break;
        win = win.parent;
      }
    } catch { /* An inaccessible ancestor cannot prove the embedding record. */ }
    observer.disconnect();
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
  if (operation === 'view_revision') {
    const ready = document.readyState === 'complete' && !document.querySelector('[aria-busy="true" i]');
    return result(ready && args.realm === state.realm && args.restoration === state.restoration && args.url === location.href ? state.revision : null);
  }
  if (operation === 'visual_revision') return result(state.revision);
  if (operation === 'stamp') return result(stamp());
  if (operation === 'ready') return result(document.readyState === 'complete');
  if (operation === 'stats') return result({nodes:state.nodes.size,scans:state.scan?1:0,pending:state.scan?.pending.length||0});
  if (operation === 'clear_nodes') {if(args.realm!==state.realm||args.restoration!==state.restoration)return result(false);state.clearNodes();return result(true);}
  if (operation === 'open') {
    retain();
    state.clearNodes();
    state.scan = {pending:document.documentElement ? [document.documentElement] : [],query:args,revision:state.revision,sequence:0,scanned:0};
    return result(stamp());
  }
  if (operation === 'next') {
    const scan = state.scan;
    if (!scan || args.realm !== state.realm || args.restoration !== state.restoration || args.sequence !== scan.sequence) return result(null);
    retain();
    state.clearNodes();
    const rows = [], omitted = new Set(['closed_shadow_not_observable','canvas_pixels_outside_profile','browser_ui_outside_profile']);
    let count = 0, characters = 0;
    while (scan.pending.length && rows.length < args.nodes && count < 5000) {
      const node = scan.pending.pop(); count++; scan.scanned++;
      if (node.nodeType === Node.TEXT_NODE) {
        if (scan.query.filter==='interactive' || scan.query.filter==='drag_endpoints' || !norm(node.nodeValue) || !node.parentElement || !visible(node.parentElement)) continue;
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
      const endpoints = scan.query.filter === 'drag_endpoints';
      const mapped = endpoints ? null : role(node);
      const inViewport = () => { const box=node.getBoundingClientRect(); return box.bottom>0 && box.right>0 && box.top<innerHeight && box.left<innerWidth; };
      const ordinary = mapped && mapped.role && (scan.query.filter!=='interactive'||controls.has(mapped.role)||mapped.role==='heading'||mapped.role==='inferred'||mapped.kind==='scrollable');
      const row = endpoints ? endpointFacts(node) :
        ordinary && visible(node) && (scan.query.filter !== 'viewport' || inViewport()) ? facts(node, mapped) : null;
      if (row) {
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
    try { rememberSubmitPairs(rows); } catch { /* Optional for ordinary observations. */ }
    scan.sequence++;
    if(!scan.pending.length)state.scan=null;
    return result({stamp:stamp(),revision:scan.revision,rows,sequence:scan.sequence,more:scan.pending.length>0,scanned:count,
      changed:scan.revision !== state.revision,omitted:Array.from(omitted).sort(),characters});
  }
  if (operation === 'reusable_submit') {
    const radio = target();
    const guard = state.reuse, before = guard?.pairs.get(radio);
    if (guard && guard.observer.takeRecords().length) guard.invalid = true;
    if (!before || !radio.checked || !radio.isConnected) return result(null);
    const {submit, form} = before;
    if (!submit.isConnected || radio.form !== form || submit.form !== form || disabled(submit))
      return result(null);
    return result(!guard.invalid && guard.documents.every(([doc, url]) => doc.URL === url) &&
      before.radios.every(([item, checked]) =>
        item.checked === (item === radio || (item.name !== radio.name && checked))) ? identity(submit) : null);
  }
  const activeElement=()=>{let node=document.activeElement;while(node?.shadowRoot?.activeElement)node=node.shadowRoot.activeElement;return node;};
  const viewportHit=(x,y)=>{
    let node=document.elementFromPoint(x,y);
    for(let depth=0;node?.shadowRoot&&depth<64;depth++){
      const child=node.shadowRoot.elementFromPoint(x,y);if(!child||child===node)break;node=child;
    }
    return node;
  };
  if (operation === 'visual_path') {
    const surface = viewportHit(args.x, args.y);
    if (!surface) return result(false);
    // Include the destination and sample the native pointer's straight path.
    const distance = Math.max(Math.abs(args.to_x - args.x), Math.abs(args.to_y - args.y));
    const steps = Math.max(1, Math.ceil(distance / 4));
    for (let i = 1; i <= steps; i++) {
      const x = args.x + (args.to_x - args.x) * i / steps;
      const y = args.y + (args.to_y - args.y) * i / steps;
      if (viewportHit(x, y) !== surface) return result(false);
    }
    return result(true);
  }
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
      if(controls.has(role(item).role)||item.localName==='label'||args.endpoint&&endpointMarker(item))return result(false);
    }
    return result(false);
  }
  if (operation === 'recheck') {
    if(args.realm!==state.realm||args.restoration!==state.restoration)return result(args.nodes.map(()=>null));
    return result(args.nodes.map(id => args.endpoint ? endpointFacts(state.nodes.get(id)) : facts(state.nodes.get(id))));
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
  if (operation === 'submit_probe_arm') {
    const node = target();
    if (!node?.isConnected) return result(false);
    const isSubmitter = (node.localName === 'button' && node.type === 'submit') ||
      (node.localName === 'input' && ['submit', 'image'].includes(node.type));
    const root = node.getRootNode();
    const isEnterTarget = args.enter && root.activeElement === node && node.localName === 'input' &&
      ['text', 'search', 'tel', 'url', 'email', 'password', 'date', 'month', 'week',
       'time', 'datetime-local', 'number'].includes(node.type);
    const form = node.form;
    if ((!isSubmitter && !isEnterTarget) || !form || form.noValidate ||
        (isSubmitter && node.formNoValidate)) return result(false);
    state.submitProbe?.stop();
    const probe = {submitter:node, event:null};
    const listener = event => {
      const field = event.target;
      if (probe.event || !event.isTrusted || field?.form !== form || field.getRootNode() !== root) return;
      const validation = state.nativeValidation(field);
      probe.event = validation.state === 'invalid' ? validation : {state:'omitted', reasons:[]};
      if (args.register_field && validation.state === 'invalid') probe.field = new WeakRef(field);
    };
    probe.stop = () => {
      root.removeEventListener('invalid', listener, true);
      clearTimeout(probe.timer);
      if (state.submitProbe === probe) state.submitProbe = null;
    };
    root.addEventListener('invalid', listener, true);
    probe.timer = setTimeout(probe.stop, 1000);
    state.submitProbe = probe;
    return result(true);
  }
  if (operation === 'release') { state.clearNodes(); state.scan=null;clearTimeout(state.timer);state.timer=null;state.expires=0;return result(true); }
  throw new Error('Unknown fixed semantic operation');
}
