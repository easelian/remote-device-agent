// Closed DOM evidence program. It shares only the document stamp with the
// canonical observer; its registry contains WeakRefs and never action nodes.
function(operation, args) {
  'use strict';
  const canonical = globalThis.__rdhSemanticDomV1;
  const result = value => JSON.stringify(value);
  if (!canonical || canonical.document !== document || canonical.realm !== args.realm ||
      canonical.restoration !== args.restoration) return result(null);
  const SLOT = '__rdhDomEvidenceV1', now = performance.now();
  let registry = globalThis[SLOT];
  if (!registry || registry.document !== document || registry.realm !== args.realm ||
      registry.restoration !== args.restoration) {
    registry = {document, realm:args.realm, restoration:args.restoration,
      serial:0, nodes:new Map()};
    Object.defineProperty(globalThis, SLOT, {value:registry, configurable:true});
  }
  for (const [id, entry] of registry.nodes) {
    if (now - entry.created >= 60000 || !entry.weak.deref()) registry.nodes.delete(id);
  }
  const remember = (node, keep=false) => {
    for (const [id, entry] of registry.nodes) {
      if (entry.weak.deref() === node) {
        if (keep) { registry.nodes.delete(id); registry.nodes.set(id,entry); }
        return id;
      }
    }
    if (registry.nodes.size >= 128) registry.nodes.delete(registry.nodes.keys().next().value);
    const id = ++registry.serial;
    if (!Number.isSafeInteger(id)) throw new Error('Evidence identity exhausted');
    registry.nodes.set(id,{weak:new WeakRef(node),created:now});
    return id;
  };
  if (operation === 'submit_probe_read') {
    const probe = canonical.submitProbe;
    if (!probe) return result(null);
    probe.stop();
    const submitter = canonical.nodes.get(args.node);
    if (!probe.event || submitter !== probe.submitter) return result(null);
    const field = probe.field?.deref();
    if (!field?.isConnected || field.form !== submitter.form ||
        field.getRootNode() !== submitter.getRootNode()) return result({validation:probe.event});
    if (canonical.sensitiveControl(field)) return result({validation:{state:'omitted', reasons:[]}});
    return result({validation:probe.event, field_node:remember(field)});
  }
  const limits = new Set(), omitted = new Set();
  let elements = 0, texts = 0, others = 0;
  const visit = node => {
    if (node.nodeType === Node.ELEMENT_NODE) {
      if (elements >= args.elements) { limits.add('element_limit'); return false; }
      elements++;
    } else if (node.nodeType === Node.TEXT_NODE) {
      if (texts >= args.text_nodes) { limits.add('text_node_limit'); return false; }
      texts++;
    } else {
      if (others >= args.other_nodes) { limits.add('other_node_limit'); return false; }
      others++;
    }
    return true;
  };
  const prefix = (value, maximum, reason) => {
    let text = '', count = 0;
    for (const scalar of String(value || '')) {
      if (count++ >= maximum) { limits.add(reason); break; }
      text += scalar;
    }
    return text.toWellFormed();
  };
  const normalized = (value, maximum, reason) => prefix(value,maximum,reason)
    .replace(/[\t\n\f\r ]+/g,' ').trim();
  const parent = node => node.parentElement || node.getRootNode().host || null;
  const sensitiveControl = node => canonical.sensitiveControl(node, () => limits.add('attribute_limit'));
  const sensitive = node => node.localName === 'input' && sensitiveControl(node);
  // Style guards inspect only ordinary parentElement ancestors. This stops at
  // shadow-root and slot boundaries and is distinct from semantic parent().
  const styleSensitive = node => {
    const control = node.localName === 'input' || node.localName === 'textarea' || node.localName === 'select';
    return (control || node.isContentEditable) && sensitiveControl(node);
  };
  const directStyleSensitive = node => {
    const formControl = node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement || node instanceof HTMLSelectElement;
    return formControl && sensitiveControl(node);
  };
  const styleTargetSensitive = node => {
    for (let item=node, depth=0; item && depth<=4; item=item.parentElement, depth++) {
      if (styleSensitive(item)) return true;
    }
    return false;
  };
  const nativeValidation = node => canonical.nativeValidation(node, () => limits.add('attribute_limit'));
  const excludesText = node => ['input','textarea','select','script','style','template','noscript'].includes(node.localName) || node.isContentEditable;
  function textAllowed(node) {
    for (let item=node,depth=0; item; item=parent(item),depth++) {
      if (depth >= 64) { limits.add('depth_limit'); return false; }
      if (!visit(item)) return false;
      if (excludesText(item)) return false;
    }
    return true;
  }
  // A bounded iterator retains at most one sibling cursor per depth. Neither
  // childNodes nor querySelectorAll is expanded into an unbounded collection.
  function walk(root, descendShadow, callback, textOnly=false) {
    const stack = [{next:root.nodeType === Node.DOCUMENT_NODE ? root.documentElement : root.firstChild,depth:0}];
    while (stack.length) {
      const cursor=stack[stack.length-1], node=cursor.next;
      if (!node) { stack.pop(); continue; }
      cursor.next=node.nextSibling;
      if (!visit(node)) return;
      if (callback(node) === false) return;
      if (node.nodeType !== Node.ELEMENT_NODE || (textOnly && excludesText(node))) continue;
      if (cursor.depth >= 64) {
        if (node.firstChild || node.shadowRoot?.firstChild) limits.add('depth_limit');
        continue;
      }
      // Light DOM precedes open shadow children. Shadow roots do not expose
      // closed roots and do not cross a frame document.
      if (descendShadow && node.shadowRoot?.firstChild) stack.push({next:node.shadowRoot.firstChild,depth:cursor.depth+1});
      if (node.firstChild) stack.push({next:node.firstChild,depth:cursor.depth+1});
    }
  }
  function textOf(node, maximum, reason, recursive=false) {
    if (!textAllowed(node)) return '';
    let text='', count=0, done=false;
    const append = item => {
      if (item.nodeType !== Node.TEXT_NODE) return;
      // substringData avoids materializing an entire oversized Text value.
      const chunk=item.substringData(0,2*(maximum-count)+2);
      for (const scalar of chunk) {
        if (count >= maximum) { limits.add(reason); done=true; break; }
        text+=scalar; count++;
      }
      if (!done && item.length>chunk.length) { limits.add(reason); done=true; }
      return done ? false : undefined;
    };
    if (recursive) walk(node,true,append,true);
    else {
      for (let child=node.firstChild; child; child=child.nextSibling) {
        if (!visit(child) || append(child) === false) break;
      }
    }
    return text.toWellFormed().replace(/[\t\n\f\r ]+/g,' ').trim();
  }
  const idIndexes = new Map();
  function indexed(root) {
    if (idIndexes.has(root)) return idIndexes.get(root);
    const index=new Map(); idIndexes.set(root,index);
    walk(root,false,node=>{
      if (node.nodeType !== Node.ELEMENT_NODE || !node.hasAttribute('id')) return;
      const id=prefix(node.getAttribute('id'),256,'name_limit');
      if (!id) return;
      // Null denotes ambiguity. Keep no per-duplicate list.
      index.set(id,index.has(id)?null:node);
    });
    return index;
  }
  function nameOf(node) {
    let associations=0;
    const resolve = (item,path,depth) => {
      if (depth>=16 || path.has(item)) { omitted.add('unresolved_name_reference'); return ''; }
      const ids=normalized(item.getAttribute('aria-labelledby'),4096,'name_limit');
      if (!ids) return textOf(item,256,'name_limit');
      const next=new Set(path); next.add(item);
      let name='';
      for (const id of ids.split(' ')) {
        if (++associations>32) { limits.add('name_limit'); omitted.add('unresolved_name_reference'); break; }
        const target=indexed(item.getRootNode()).get(id);
        if (!target || !visit(target)) { omitted.add('unresolved_name_reference'); continue; }
        const value=resolve(target,next,depth+1);
        if (!value) omitted.add('unresolved_name_reference');
        name=normalized(name+(name?' ':'')+value,256,'name_limit');
      }
      return name;
    };
    let name='', source='none';
    if (normalized(node.getAttribute('aria-labelledby'),4096,'name_limit')) {
      name=resolve(node,new Set(),0); if (name) source='aria_labelledby';
    }
    if (!name) { name=normalized(node.getAttribute('aria-label'),256,'name_limit'); if (name) source='aria_label'; }
    if (!name && node.labels) {
      for (const label of node.labels) {
        if (++associations>32) { limits.add('name_limit'); omitted.add('unresolved_name_reference'); break; }
        if (!visit(label)) { omitted.add('unresolved_name_reference'); break; }
        name=normalized(name+(name?' ':'')+textOf(label,256,'name_limit',true),256,'name_limit');
      }
      if (name) source='label';
    }
    if (!name) { name=textOf(node,256,'name_limit'); if (name) source='direct_text'; }
    return {name,name_source:source};
  }
  const roleOf = node => {
    const explicit=prefix(node.getAttribute('role'),4096,'attribute_limit').split(/[\t\n\f\r ]+/)
      .find(token=>/^[a-z][a-z0-9_-]{0,63}$/.test(token));
    if (explicit) return explicit;
    const tag=node.localName;
    if (tag==='button'||tag==='summary') return 'button';
    if (tag==='a'&&node.hasAttribute('href')) return 'link';
    if (tag==='textarea') return 'textbox';
    if (tag==='select') return 'combobox';
    if (tag==='img') return 'img';
    if (/^h[1-6]$/.test(tag)) return 'heading';
    if (tag==='input') {
      const type=node.type;
      if (type==='hidden') return '';
      if (['button','submit','reset','image'].includes(type)) return 'button';
      if (['checkbox','radio'].includes(type)) return type;
      return type==='range'?'slider':type==='number'?'spinbutton':type==='search'?'searchbox':'textbox';
    }
    return '';
  };
  const facts = node => ({node:remember(node),tag:prefix(node.localName,64,'attribute_limit'),
    role:roleOf(node),...nameOf(node),sensitive:sensitive(node)});
  const coverage = () => ({visited_elements:elements,visited_text_nodes:texts,visited_other_nodes:others,
    truncated:limits.size>0,limits:[...limits].sort(),omitted:[...omitted].sort()});
  function allowsRecordText(element) {
    if (excludesText(element) || element.hidden || element.inert ||
        element.getAttribute('aria-hidden') === 'true') return false;
    const style = getComputedStyle(element);
    return style.display !== 'none' && style.visibility !== 'hidden' && style.visibility !== 'collapse'
      && style.opacity !== '0' && style.contentVisibility !== 'hidden';
  }
  function matchesRecord(record, baseline = false) {
    if (canonical.recordKind(record) !== 'record') return false;
    for (let item = record, depth = 0; item; item = parent(item), depth++) {
      if (depth >= 64) { limits.add('depth_limit'); return false; }
      if (!visit(item) || !allowsRecordText(item)) return false;
    }
    const stack = [{next:record.firstChild}];
    let visited = 0, characters = 0, anchors = 0, expected = false;
    while (stack.length) {
      const cursor = stack[stack.length - 1], item = cursor.next;
      if (!item) { stack.pop(); continue; }
      cursor.next = item.nextSibling;
      if (++visited > 256) { limits.add('element_limit'); return false; }
      if (!visit(item)) return false;
      if (item.nodeType === Node.ELEMENT_NODE) {
        if (canonical.recordKind(item) || !allowsRecordText(item)) continue;
        if (item.firstChild) {
          if (stack.length >= 64) { limits.add('depth_limit'); return false; }
          stack.push({next:item.firstChild});
        }
      } else if (item.nodeType === Node.TEXT_NODE) {
        const raw = prefix(item.data, 4096 - characters, 'excerpt_limit');
        if (limits.size) return false;
        characters += Array.from(raw).length;
        const text = raw.replace(/[\t\n\f\r ]+/g,' ').trim();
        if (text !== args.anchor_text && text !== args.expected_text) continue;
        const range = document.createRange();
        range.selectNodeContents(item);
        const rect = range.getBoundingClientRect();
        if (rect.width > 0 && rect.height > 0) {
          if (text === args.anchor_text) anchors++;
          expected ||= text === args.expected_text;
        }
      }
    }
    return baseline ? anchors === 1 && !expected : anchors > 0 && expected;
  }
  // Feedback is optional. Use a complete bounded snapshot on both sides of
  // the input and never promote a generic live region to a rejection.
  function owner(node) {
    for (let depth=0; node && depth<64; node=node.parentElement, depth++) {
      if (canonical.recordKind(node)) return node;
    }
    return null;
  }
  const editable = node => node.matches('input,select,textarea,button,option,[contenteditable]:not([contenteditable="false"])') || node.isContentEditable;
  function feedbackVisible(node, record, field = false) {
    if (!node?.isConnected || owner(node) !== record) return false;
    for (let item=node, depth=0; item; item=parent(item), depth++) {
      if (depth>=64 || item.hidden || item.inert || item.getAttribute('aria-hidden')==='true' ||
          (editable(item) && !(field && item===node))) return false;
      const style=getComputedStyle(item);
      if (style.display==='none' || ['hidden','collapse'].includes(style.visibility) ||
          style.opacity==='0' || style.contentVisibility==='hidden') return false;
    }
    return !field || node.getClientRects().length>0;
  }
  function feedbackText(node, record, links, excluded) {
    if (node.shadowRoot || !feedbackVisible(node,record) || excludesText(node) || editable(node)) return null;
    let text='', safe=true;
    walk(node,false,item=>{
      if (excluded?.has(item)) { safe=false; return false; }
      if (item.nodeType===Node.ELEMENT_NODE) {
        if (canonical.recordKind(item) || links.has(item.id) || !feedbackVisible(item,record) ||
            excludesText(item) || editable(item) || item.matches('a,iframe,[role],slot') || item.shadowRoot) {
          safe=false; return false;
        }
      } else if (item.nodeType===Node.TEXT_NODE) {
        if (item.length>512 || text.length+item.length>512) { safe=false; return false; }
        const value=item.data;
        if (value.trim()) {
          const range=document.createRange();
          range.selectNodeContents(item);
          const rect=range.getBoundingClientRect();
          if (!rect.width || !rect.height) { safe=false; return false; }
        }
        text+=value;
      }
    });
    text=text.replace(/[\t\n\r\f ]+/g,' ').trim();
    if (!safe || limits.size || !text || Array.from(text).length>256 ||
        /[\p{Cc}\p{Cf}\u2028\u2029]/u.test(text)) return null;
    return text;
  }
  const messageSelector='[role="alert"],[role="status"],p,output';
  function plainMessage(node) {
    return node.matches('div,span') && !node.firstElementChild &&
      !node.parentElement?.closest('label,button,'+messageSelector);
  }
  function feedbackSnapshot(record,form,baseline=false) {
    // HTMLFormElement named controls can shadow DOM methods. Diagnostics are
    // optional even if this page cannot provide a complete snapshot.
    try { return collectFeedback(record,form,baseline); }
    catch { return null; }
  }
  function collectFeedback(record,form,baseline) {
    if (!record.isConnected || !form.isConnected || owner(form)!==record) return null;
    const root=form.getRootNode();
    // IDREFs are scoped to a tree root, and any control can refer to the
    // same message. An ambiguous or foreign association cannot name a field.
    const ids=new Map(), links=new Map(), candidates=new Set();
    let protectedInvalid=false;
    walk(root,false,node=>{
      if (node.nodeType!==Node.ELEMENT_NODE) return;
      const id=prefix(node.id,256,'attribute_limit');
      if (id) ids.set(id,ids.has(id)?null:node);
      // Baseline all visible short text containers, even before a page assigns
      // an alert role or points a field at an existing help message.
      if (owner(node)===record && (baseline || node.matches(messageSelector) || plainMessage(node))) {
        candidates.add(node);
      }
      if (!node.matches('input,select,textarea')) return;
      const own=owner(node)===record && node.form===form;
      const invalid=node.getAttribute('aria-invalid')==='true';
      const guarded=own && sensitiveControl(node);
      if (guarded && invalid) protectedInvalid=true;
      const err=normalized(node.getAttribute('aria-errormessage'),512,'attribute_limit');
      const desc=normalized(node.getAttribute('aria-describedby'),512,'attribute_limit');
      const preferred=(err || desc).split(' ').filter(Boolean);
      for (const key of new Set((err+' '+desc).split(' ').filter(Boolean))) {
        const field=own && !guarded && preferred.length===1 && preferred[0]===key &&
          feedbackVisible(node,record,true) ? node : null;
        links.set(key,links.has(key)?null:{field,invalid});
      }
    });
    if (limits.size || protectedInvalid) return null;
    for (const [key,link] of links) {
      const node=ids.get(key);
      if (link?.field && node && owner(node)===record) candidates.add(node);
    }
    const found=[];
    for (const node of candidates) {
      const containingForm=node.closest('form');
      if (containingForm && containingForm!==form) continue;
      let field=null, kind=null;
      // Content owned by a foreign, protected, or ambiguous control must
      // not reappear as a generic alert, even through a nested text wrapper.
      let ancestor=node;
      while (ancestor && ancestor!==record && !links.has(ancestor.id)) ancestor=ancestor.parentElement;
      if (ancestor && ancestor!==record) {
        const link=links.get(ancestor.id);
        if (ancestor!==node || !link?.field || ids.get(node.id)!==node) continue;
        field=link.field;
        if (link.invalid) kind='field_error';
      } else if (node.getAttribute('role')==='alert') kind='alert';
      else if (node.getAttribute('role')==='status' || node.localName==='output') kind='status';
      else if (node.localName==='p' || plainMessage(node)) kind='record_text';
      const text=feedbackText(node,record,links);
      if (text) found.push({node,field,kind,text});
      if (found.length>32 || limits.size) return null;
    }
    return {found,rejected:[record,form].some(n=>n.getAttribute('data-submit-state')==='rejected')};
  }
  // Only normalized paint colors leave the device. An unsupported paint
  // (undefined) is distinct from a transparent or absent paint (null).
  function paintColor(css) {
    if (css==='transparent') return null;
    const value=/^rgba?\(([^()]*)\)$/.exec(css);
    if (!value) return undefined;
    const body=value[1].trim();
    const tokens=body.includes(',') ? body.split(',').map(x=>x.trim()) :
      body.replace(/\s*\/\s*/, ' ').split(/\s+/);
    if (tokens.length!==3 && tokens.length!==4) return undefined;
    const bytes=[];
    for (let i=0;i<tokens.length;i++) {
      const token=tokens[i];
      if (!/^\d*\.?\d+%?$/.test(token)) return undefined;
      const max=token.endsWith('%')?100:(i===3?1:255);
      const n=parseFloat(token);
      if (n>max) return undefined;
      bytes.push(Math.round(n/max*255));
    }
    if (bytes[3]===0) return null;
    if (bytes.length===3) bytes.push(255);
    return '#'+bytes.map(n=>n.toString(16).padStart(2,'0')).join('');
  }
  function fieldAppearance(field, counts) {
    const style=getComputedStyle(field), paint={};
    for (const side of ['top','right','bottom','left','outline']) {
      const key=side==='outline'?side:'border_'+side;
      const prop=key.replace('_','-');
      if (side!=='outline' && style.borderImageSource!=='none') continue;
      paint[key]=['none','hidden'].includes(style.getPropertyValue(prop+'-style')) ||
        parseFloat(style.getPropertyValue(prop+'-width'))===0 ? null :
        paintColor(style.getPropertyValue(prop+'-color'));
    }
    if (style.backgroundImage==='none') paint.background=paintColor(style.backgroundColor);
    // A submit button can take focus. Compare interaction on the field and
    // its own wrappers so normal focus, blur and hover paint isn't evidence.
    let interaction=field.matches(':disabled')?'d':'e';
    interaction+=field.readOnly?'r':'w';
    for (let node=field; node && (node===field || counts.get(node)===1); node=node.parentElement) {
      interaction+=String(Number(node.matches(':focus-within'))+
        2*Number(node.matches(':hover'))+4*Number(node.matches(':active')));
    }
    return {paint,interaction};
  }
  function appearanceChanges(before,after) {
    if (!before || before.interaction!==after.interaction) return [];
    return Object.keys(after.paint).filter(part=>
      after.paint[part]!==undefined && before.paint[part]!==undefined && before.paint[part]!==after.paint[part]
    ).map(part=>({part,before:before.paint[part],after:after.paint[part]}));
  }

  // Keep field attribution separate from the single-message query. Weak keys
  // retain the pre-click identities without keeping detached controls alive.
  function fieldSnapshot(record, form, previous) {
    try { return collectFields(record, form, previous); }
    catch { return null; } // Named form controls may shadow DOM methods.
  }
  function collectFields(record, form, previous) {
    if (!record.isConnected || !form.isConnected || owner(form)!==record) return null;
    const ids=new Map(), links=new Map(), controls=[], counts=new Map(), candidates=[];
    const excluded=new WeakSet(), originals=new WeakMap(), visibleText=new WeakMap();
    const oldByField=new WeakMap(), priorOwner=new WeakMap(), appearances=new WeakMap(), visual=[];
    const eligible=node=>node && owner(node)===record &&
      (!node.closest('form') || node.closest('form')===form);
    const inputSelector='input,select,textarea';
    const messageTag=node=>node.matches('small,span,p,div,output,strong,em,[role="alert"],[role="status"]');
    walk(form.getRootNode(),false,node=>{
      const element=node.nodeType===Node.ELEMENT_NODE ? node : node.parentElement;
      if (!element || !eligible(element)) excluded.add(node);
      if (node.nodeType!==Node.ELEMENT_NODE) return;
      const id=prefix(node.id,256,'attribute_limit');
      if (id) ids.set(id,ids.has(id)?null:node);
      const err=normalized(node.getAttribute('aria-errormessage'),512,'attribute_limit');
      const desc=normalized(node.getAttribute('aria-describedby'),512,'attribute_limit');
      const keys=(err || desc).split(' ').filter(Boolean);
      for (const key of new Set((err+' '+desc).split(' ').filter(Boolean))) {
        const source=keys.length===1 && keys[0]===key ? node : null;
        links.set(key,links.has(key)?null:source);
      }
      if (eligible(node) && messageTag(node)) candidates.push(node);
      if (!node.matches(inputSelector)) return;
      // Include other forms' controls and protected fields in group counts.
      for (let p=node.parentElement, d=0; p && p!==record && p!==form; p=p.parentElement) {
        if (++d>64) { limits.add('depth_limit'); return; }
        counts.set(p,1+(counts.get(p)||0));
      }
      if (eligible(node) && node.form===form) controls.push(node);
    });
    if (limits.size) return null;
    const reasons=new Map(), pairs=new Map();
    const textOfMessage=node=>{
      if (!reasons.has(node)) {
        const text=feedbackText(node,record,links,previous?.excluded);
        reasons.set(node,text);
        if (text) visibleText.set(node,text);
      }
      return reasons.get(node);
    };
    // Remember visible text before it becomes linked, including a formerly
    // generic container which the page later promotes to an error message.
    if (!previous) for (const node of candidates) textOfMessage(node);
    function add(field,node,association,safe) {
      if (!eligible(node) || excluded.has(node) || previous?.excluded.has(node)) return;
      if (priorOwner.has(node) && priorOwner.get(node)!==field) {
        priorOwner.set(node,null);
      } else if (!priorOwner.has(node)) priorOwner.set(node,field);
      if (!safe || (previous?.priorOwner.has(node) && previous.priorOwner.get(node)!==field)) return;
      for (let p=node.parentElement; p && p!==record; p=p.parentElement) {
        if (links.has(p.id)) return;
      }
      const text=textOfMessage(node);
      if (!text) return;
      if (!oldByField.has(field)) oldByField.set(field,new Set());
      oldByField.get(field).add(text);
      if (!previous || previous.visibleText.get(node)===text || previous.oldByField.get(field)?.has(text)) return;
      if (!pairs.has(field)) pairs.set(field,new Map());
      pairs.get(field).set(node,{node,text,association});
    }
    const linkedMessage=new Map();
    for (const [id,source] of links) {
      if (source && ids.get(id)) linkedMessage.set(source,ids.get(id));
    }
    for (const field of controls) {
      const identity=sensitiveControl(field)?null:JSON.stringify([field.id,field.name,field.type]);
      const safe=identity!==null && feedbackVisible(field,record,true) &&
        (!previous || previous.originals.get(field)===identity);
      if (safe) {
        originals.set(field,identity);
        const appearance=fieldAppearance(field,counts);
        if (previous) {
          const changes=appearanceChanges(previous.appearances.get(field),appearance);
          if (changes.length) visual.push({field,changes});
        } else appearances.set(field,appearance);
      }
      const invalid=field.getAttribute('aria-invalid')==='true';
      const linked=linkedMessage.get(field);
      if (linked) add(field,linked,'aria',safe && (invalid || !previous));
      // An explicit error relationship overrides proximity, even if its
      // target is outside this record, shared, or otherwise unreadable.
      if (field.hasAttribute('aria-errormessage') || (invalid && field.hasAttribute('aria-describedby'))) continue;
      // Look forward from the control and its single-control wrappers. Do
      // not cross a label for another field, a new control, or a form boundary.
      for (let part=field; part && part!==form && part!==record; part=part.parentElement) {
        const group=part.parentElement;
        if (!group || group===record || !eligible(group)) break;
        const directForm=group===form;
        if (!directForm && counts.get(group)!==1) break;
        // Bare form children are meaningful only after a labelled control or
        // a wrapper containing exactly one. This rules out paired bare inputs.
        const bareOK=part===field ? Boolean(field.labels?.length) : counts.get(part)===1;
        if (!directForm || bareOK) {
          for (let msg=part.nextElementSibling; msg && messageTag(msg); msg=msg.nextElementSibling) {
            if (counts.has(msg) || canonical.recordKind(msg) || !eligible(msg)) break;
            if (links.has(msg.id)) continue;
            const strong=invalid || msg.matches('.field-error,.error,.invalid-feedback,.error-message,[data-error="true"]');
            add(field,msg,strong?'adjacent_error':'adjacent_text',safe);
          }
        }
        if (directForm) break;
      }
    }
    if (limits.size) return null;
    if (!previous) return {originals,excluded,visibleText,oldByField,priorOwner,appearances};
    const rows=[];
    for (const [field,byNode] of pairs) {
      const owned=[...byNode.values()].filter(item=>priorOwner.get(item.node)===field);
      const marked=owned.filter(item=>item.association!=='adjacent_text');
      const choices=marked.length?marked:owned;
      if (choices.length!==1) continue;
      const {node,text,association}=choices[0];
      rows.push({field_node:remember(field,true),message_node:remember(node,true),reason:text,association});
      if (rows.length>16) break;
    }
    return {fields:rows.length<=16?rows:[],visual_changes:visual.length<=16?
      visual.map(({field,changes})=>({field_node:remember(field,true),changes})):[]};
  }
  if (operation==='submit_fields_read') {
    const empty={fields:[],visual_changes:[]};
    const baseline=registry.submitFeedback, form=baseline?.form.deref();
    const record=registry.nodes.get(args.record)?.weak.deref();
    if (!baseline?.fields || !form || !record || record!==baseline.record.deref() ||
        baseline.anchor!==args.anchor_text || !matchesRecord(record,true) || limits.size) return result(empty);
    remember(record,true);
    return result(fieldSnapshot(record,form,baseline.fields) || empty);
  }

  if (operation === 'submit_feedback_read') {
    const baseline=registry.submitFeedback;
    const record=registry.nodes.get(args.record)?.weak.deref();
    const form=baseline?.form.deref();
    if (!baseline?.texts || !record || !form || record!==baseline.record.deref() ||
        baseline.anchor!==args.anchor_text || !matchesRecord(record,true) || limits.size) return result(null);
    const current=feedbackSnapshot(record,form);
    if (!current) return result(null);
    // A pre-existing message, even with a replacement DOM node, is not fresh.
    const changed=current.found.filter(item=>item.kind && !baseline.texts.has(item.text));
    const marked=changed.filter(item=>item.kind!=='record_text');
    const choices=marked.length?marked:changed;
    if (choices.length!==1) return result(null);
    const {node,field,kind,text}=choices[0];
    if (kind==='field_error') return result({kind,field_node:remember(field),message_node:remember(node),reason:text});
    const rejection=(kind==='alert' || kind==='status') && current.rejected && !baseline.rejected;
    return result({kind:rejection?'record_rejection':kind,message_node:remember(node),reason:text});
  }
  if (operation === 'submit_record_arm' || operation === 'submit_record_read') {
    const entry = registry.nodes.get(args.record);
    const record = entry?.weak.deref();
    const submitter = canonical.nodes.get(args.node);
    if (operation === 'submit_record_read') {
      const probe = registry.submitProbe;
      if (!probe) return result(null);
      probe.stop();
      return result(record === probe.record && submitter === probe.submitter && probe.observed);
    }
    registry.submitProbe?.stop();
    if (!record?.isConnected || !submitter?.isConnected) return result(null);
    const form = submitter.form;
    const canSubmit = (submitter.localName === 'button' && submitter.type === 'submit') ||
      (submitter.localName === 'input' && ['submit','image'].includes(submitter.type));
    // Resolve submitter overrides only when the attribute is actually present.
    const endpoint = () => {
      const method = submitter.hasAttribute('formmethod') ? submitter.formMethod : form.method;
      if (method !== 'post') return null;
      const url = new URL(submitter.hasAttribute('formaction') ? submitter.formAction : form.action);
      if (!['http:', 'https:'].includes(url.protocol) || url.origin !== location.origin ||
          url.username || url.password || url.href.length > 4096) return null;
      url.hash = '';
      return url.href;
    };
    if (!canSubmit || !form || owner(form)!==record || owner(submitter)!==record ||
        !matchesRecord(record, true) || limits.size) return result(null);
    const url = endpoint();
    if (!url) return result(null);
    const probe = {submitter, record, observed:false};
    clearTimeout(registry.submitFeedback?.expiry);
    registry.submitFeedback=null;
    const snapshot=feedbackSnapshot(record,form,true);
    // The optional list has an independent budget. Exhausting either scan
    // omits only its own diagnostic evidence and cannot veto the input.
    elements=0; texts=0; others=0; limits.clear();
    const fields=fieldSnapshot(record,form,null);
    if (snapshot || fields) {
      const baseline={record:new WeakRef(record),form:new WeakRef(form),anchor:args.anchor_text,
        fields,texts:snapshot && new Set(snapshot.found.map(item=>item.text)),rejected:snapshot?.rejected};
      baseline.expiry=setTimeout(()=>{if (registry.submitFeedback===baseline) registry.submitFeedback=null;},30000);
      registry.submitFeedback=baseline;
    }
    const listener = event => {
      if (event.isTrusted && event.target === form && event.submitter === submitter &&
          submitter.form === form && owner(form)===record && owner(submitter)===record && endpoint() === url) probe.observed = true;
    };
    const root = form.getRootNode();
    probe.stop = () => {
      root.removeEventListener('submit', listener, true);
      clearTimeout(probe.timer);
      if (registry.submitProbe === probe) registry.submitProbe = null;
    };
    root.addEventListener('submit', listener, true);
    probe.timer = setTimeout(probe.stop, 2000);
    registry.submitProbe = probe;
    return result(url);
  }
  let output;
  if (operation==='find_nodes') {
    const nodes=[];
    walk(document,true,node=>{
      if (node.nodeType!==Node.ELEMENT_NODE) return;
      if (args.tag && node.localName!==args.tag) return;
      const role=roleOf(node); if (args.role && role!==args.role) return;
      const name=nameOf(node);
      if (args.text && !name.name.includes(args.text) && !textOf(node,256,'name_limit').includes(args.text)) return;
      nodes.push({node:remember(node),tag:prefix(node.localName,64,'attribute_limit'),role,...name,sensitive:sensitive(node)});
      if (nodes.length>=args.limit) { limits.add('result_limit'); return false; }
    });
    output={kind:operation,nodes,coverage:coverage()};
  } else {
    const node=registry.nodes.get(args.node)?.weak.deref();
    if (!node || !node.isConnected || node.ownerDocument!==document) return result(null);
    // CDP calls this closed operation on its retained target. Bind the same
    // ordinary parents that passed the sensitivity check in this JS turn.
    if (operation==='bind_style_ancestors') {
      if (this!==node || styleTargetSensitive(node)) return false;
      if (node.getRootNode()!==document || node.assignedSlot || node.localName==='slot') return null;
      const chain=[];
      for (let item=node.parentElement; item && chain.length<5; item=item.parentElement) {
        if (chain.length<4 && (item.assignedSlot || item.localName==='slot')) return null;
        chain.push(item);
      }
      return chain;
    }
    if (operation==='match_record') {
      output={kind:operation,matched:matchesRecord(node),coverage:coverage()};
    } else if (operation==='style_target' || operation==='ancestor_style_target') {
      const guarded = operation==='ancestor_style_target' ? styleTargetSensitive(node) : directStyleSensitive(node);
      output={kind:'style_target',sensitive:guarded,coverage:coverage()};
    } else if (!visit(node)) {
      return result(null);
    } else if (operation==='read_node') {
      const attributes={};
      for (const key of ['type','aria-hidden','aria-disabled','aria-expanded','aria-selected','hidden','inert','disabled','readonly']) {
        if (node.hasAttribute(key)) attributes[key]=prefix(node.getAttribute(key),64,'attribute_limit');
      }
      const detail={facts:facts(node),text_excerpt:textOf(node,1024,'excerpt_limit',true),
        states:{connected:true,disabled:node.matches(':disabled')||node.getAttribute('aria-disabled')==='true'||!!node.closest('[inert]'),
          readonly:!!node.readOnly||node.getAttribute('aria-readonly')==='true',
          editable:node.isContentEditable||node.localName==='textarea'||node.localName==='input'&&!['button','submit','reset','checkbox','radio','file','hidden','image'].includes(node.type)},attributes};
      output={kind:operation,node:detail,coverage:coverage()};
    } else if (operation==='read_validation') {
      output={kind:operation,validation:nativeValidation(node),coverage:coverage()};
    } else if (operation==='read_layout') {
      const rect=node.getBoundingClientRect(),style=getComputedStyle(node),computed={};
      for (const key of ['display','visibility','opacity','pointer-events','position','z-index','overflow-x','overflow-y','width','height','transform','font-size','line-height','color','background-color']) {
        computed[key]=prefix(style.getPropertyValue(key),128,'style_limit');
      }
      const x=rect.x+rect.width/2,y=rect.y+rect.height/2;
      let hit={kind:'indeterminate'};
      if (x<0||y<0||x>=innerWidth||y>=innerHeight) hit={kind:'outside_viewport'};
      else if (rect.width>0&&rect.height>0) {
        let top=document.elementFromPoint(x,y);
        for(let depth=0;top?.shadowRoot;depth++) {
          if(depth>=64){limits.add('depth_limit');top=null;break;}
          const inner=top.shadowRoot.elementFromPoint?.(x,y);
          if(!inner||inner===top)break;
          if(!visit(inner)){top=null;break;} top=inner;
        }
        if(top===node)hit={kind:'target'};
        else if(top) {
          let descendant=false,known=true;
          for(let item=top,depth=0;item;item=parent(item),depth++) {
            if(depth>=64){limits.add('depth_limit');known=false;break;}
            if(!visit(item)){known=false;break;}
            if(item===node){descendant=true;break;}
          }
          if(known)hit=descendant?{kind:'descendant'}:{kind:'other',node:remember(top)};
        }
      }
      output={kind:operation,layout:{box:{x:rect.x,y:rect.y,width:rect.width,height:rect.height},
        viewport:{width:innerWidth,height:innerHeight},computed,hit},coverage:coverage()};
    } else throw new Error('Unknown fixed evidence operation');
  }
  // Bound the native value before transport. The runtime also measures the
  // final result after replacing serials with public identifiers and context.
  let encoded=result(output);
  while(new TextEncoder().encode(encoded).length>12000 && output.nodes?.length) {
    output.nodes.pop(); limits.add('byte_limit'); output.coverage=coverage(); encoded=result(output);
  }
  if(new TextEncoder().encode(encoded).length>16384) throw new Error('Evidence result limit');
  return encoded;
}
