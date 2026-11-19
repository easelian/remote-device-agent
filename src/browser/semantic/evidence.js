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
        2*Number(node.matches(':hover'))+4*Number(node.matches(':active'))+8*Number(node.matches(':focus-visible')));
    }
    return {paint,interaction};
  }
  function appearanceChanges(before,after) {
    if (!before || before.interaction!==after.interaction) return [];
    return Object.keys(after.paint).filter(part=>
      after.paint[part]!==undefined && before.paint[part]!==undefined && before.paint[part]!==after.paint[part]
    ).map(part=>({part,before:before.paint[part],after:after.paint[part]}));
  }

  // An aria-hidden glyph can still be painted. Expose only its evidence
  // handle and kind. Its enclosing field must pass the normal visibility check.
  function paintedIcon(node,wrapper) {
    const rect=node.getBoundingClientRect();
    if (!rect.width || !rect.height) return false;
    for (let part=node; part && part!==wrapper; part=part.parentElement) {
      const style=getComputedStyle(part);
      if (part.hidden || part.inert || style.display==='none' || style.visibility!=='visible' ||
          style.opacity==='0' || style.contentVisibility==='hidden') return false;
    }
    return feedbackVisible(wrapper,owner(wrapper));
  }
  function validationIcon(node) {
    if (!node.matches('i,img,svg,[role="img"],span[class]')) return false;
    const classes=node.getAttribute('class') || '';
    if (node.localName==='span' && node.getAttribute('role')!=='img' &&
        !/(?:^|[\s_-])(?:icon|glyph)(?:$|[\s_-])/i.test(classes)) return false;
    const marker=[classes,node.getAttribute('aria-label'),node.getAttribute('alt'),node.getAttribute('title')].join(' ');
    if (marker.length>512 || /(?:no[-_ ]error|error[-_ ]free|(?:^|[\s_-])(?:success|valid|check|info|help)(?:$|[\s_-]))/i.test(marker)) return false;
    return /(?:^|[\s_-])(?:error|invalid|warning|alert|exclamation)(?:$|[\s_-])|错误|无效|警告/i.test(marker);
  }
  function shadowSignature(style) {
    // Keep CSS private. Compare the visible layers including their geometry.
    const css=style.boxShadow;
    if (css==='none') return '';
    if (css.length>1024) return undefined;
    const layers=css.split(/,(?![^()]*\))/), painted=[];
    for (const layer of layers) {
      const rgb=layer.match(/rgba?\([^()]*\)/);
      if (!rgb) return undefined;
      const color=paintColor(rgb[0]);
      if (color===undefined) return undefined;
      const geometry=layer.replace(rgb[0],'').trim().split(/\s+/);
      if (color && geometry.some(token=>token.endsWith('px') && parseFloat(token)!==0)) painted.push(layer.trim());
    }
    return painted.join(',');
  }
  function indicatorPaint(node, field) {
    const style=getComputedStyle(node);
    const paint=Object.create(null);
    paint[node===field?'field_shadow':'wrapper_shadow']=shadowSignature(style);
    if (node!==field) {
      for (const [kind,parts] of [['wrapper_border',['top','right','bottom','left']],['wrapper_outline',['outline']]]) {
        if (kind==='wrapper_border' && style.borderImageSource!=='none') continue;
        const sides=[];
        for (const part of parts) {
          const prop=part==='outline'?'outline':'border-'+part;
          const width=style.getPropertyValue(prop+'-width'), line=style.getPropertyValue(prop+'-style');
          const color=paintColor(style.getPropertyValue(prop+'-color'));
          if (color===undefined) break;
          sides.push(color && parseFloat(width)>0 && !['none','hidden'].includes(line) ? [width,line,color].join(' ') : '');
        }
        if (sides.length===parts.length) paint[kind]=sides.some(Boolean)?sides.join('|'):'';
      }
      if (style.backgroundImage==='none') paint.wrapper_background=paintColor(style.backgroundColor);
    }
    return paint;
  }
  function indicatorSources(field,form,record,counts,iconCandidates,appearance,previous,baseline,found) {
    if (!form.contains(field)) return;
    const old=previous?.indicatorFields.get(field);
    if (previous && !old) return;
    const sameInteraction=old?.interaction===appearance.interaction;
    const paint=new WeakMap(), icons=new WeakSet(), hiddenIcons=new WeakSet(), wrappers=[];
    for (let node=field, depth=0; node && depth<=4 && node!==form && node!==record; node=node.parentElement,depth++) {
      if (node!==field && (counts.get(node)!==1 || owner(node)!==record || node.closest('form')!==form)) break;
      if (node!==field) wrappers.push(node);
      if (!feedbackVisible(node,record,node===field) || !node.getClientRects().length) continue;
      const current=indicatorPaint(node,field), before=old?.paint.get(node);
      paint.set(node,current);
      if (before && sameInteraction) for (const kind of Object.keys(current)) {
        if (current[kind] && before[kind]!==undefined && before[kind]!==current[kind]) found.push({field,source:node,kind});
      }
    }
    const wrapper=wrappers.at(-1);
    if (wrapper) for (const icon of iconCandidates) {
      if (!wrapper.contains(icon) || !validationIcon(icon) || !paintedIcon(icon,wrapper) ||
          icon.closest('form')!==form || icon.closest('a,button,[contenteditable="true"]') ||
          !icon.getClientRects().length) continue;
      icons.add(icon);
      if (!old || old.icons.has(icon)) continue;
      // Pointer and focus can move while validation inserts or unhides a glyph.
      // A CSS-only visibility change at that moment cannot establish its cause.
      if (!sameInteraction && previous.existing.has(icon) && !old.hiddenIcons.has(icon)) continue;
      // A known source must still belong to its original single-field wrapper.
      if (previous.existing.has(icon) && previous.indicatorOwners.get(icon)!==field) continue;
      found.push({field,source:icon,kind:'icon'});
    }
    baseline.indicatorFields.set(field,{paint,icons,hiddenIcons,interaction:appearance.interaction});
    // Remember unmarked and hidden elements too. Moving an old element from a
    // different field and then marking it must not manufacture a new indicator.
    if (!previous && wrapper) for (const node of iconCandidates) {
      if (wrapper.contains(node)) {
        baseline.indicatorOwners.set(node,field);
        if (node.closest('[hidden]')) hiddenIcons.add(node);
      }
    }
  }

  const summaryMarker='.error-summary,.validation-summary-errors,[data-error-summary]:not([data-error-summary="false"])';
  const summarySelector=summaryMarker+',.form-summary,[role="alert"],[role="status"],ul,ol';
  function fieldSummary(node,form,record) {
    if (!node || !form.contains(node)) return null;
    // A list item used solely to present a summary entry may sit inside a
    // record. An independently marked business record is always a boundary.
    let listItem=null;
    for (let part=node,depth=0; part && part!==form && depth<64; part=part.parentElement,depth++) {
      if (canonical.recordKind(part)) {
        if (listItem || part.localName!=='li' || part.matches('[data-rdh-record],[role],article,tr') ||
            !part.parentElement?.matches('ol,ul')) return null;
        listItem=part;
      }
      if (part.matches(summarySelector) && owner(part)===record && part.closest('form')===form) {
        if (part.matches('ul,ol')) {
          const marked=part.parentElement?.closest(summaryMarker+',.form-summary,[role="alert"],[role="status"]');
          if (marked && form.contains(marked) && owner(marked)===record) return marked;
          if (part.parentElement!==form) continue;
        }
        return part;
      }
      if ((part===form.firstElementChild || part===form.lastElementChild) &&
          part.matches('div,section,p,small') && owner(part)===record) return part;
    }
    return null;
  }
  function summaryLine(node,box,previous,link,links) {
    for (let item=node,depth=0; item; item=parent(item),depth++) {
      if (depth>=64 || editable(item) || !allowsRecordText(item) ||
          (box.contains(item) && (previous?.foreign.has(item) || links.has(item.id)))) return null;
      if (item===box && !item.isConnected) return null;
    }
    let text='', valid=true;
    walk(node,false,child=>{
      if (previous?.foreign.has(child)) { valid=false; return false; }
      if (child.nodeType===Node.ELEMENT_NODE) {
        if (links.has(child.id) || (child!==link && !child.matches('span,strong,em,b,i,code')) || !allowsRecordText(child) ||
            editable(child) || child.shadowRoot) { valid=false; return false; }
      } else if (child.nodeType===Node.TEXT_NODE) {
        if (child.length>512 || child.length+text.length>512) { valid=false; return false; }
        if (child.data.trim()) {
          const range=document.createRange(); range.selectNodeContents(child);
          const rect=range.getBoundingClientRect();
          if (!rect.width || !rect.height) { valid=false; return false; }
        }
        text+=child.data;
      }
    });
    text=text.replace(/[\t\n\r\f ]+/g,' ').trim();
    return valid && text && Array.from(text).length<=256 && !/[\p{Cc}\p{Cf}\u2028\u2029]/u.test(text)?text:null;
  }
  function summaryFeedback(record,controls,safeFields,entries,ids,links,counts,old,now,pairs) {
    const labels=new Map(), names=new Map();
    for (const field of controls) {
      const explicit=normalized(field.getAttribute('aria-label'),256,'attribute_limit');
      let label=explicit;
      if (!label && field.labels?.length===1) {
        const node=field.labels[0];
        if (owner(node)===record && feedbackVisible(node,record)) label=textOf(node,256,'name_limit',true);
      }
      if (label) {
        names.set(field,label);
        labels.set(label,labels.has(label)?null:field);
      }
    }
    for (const [node,box] of entries) {
      if (counts.has(box)) continue;
      let link=node.localName==='a' ? node : null;
      if (node.localName==='li') {
        const child=node.firstElementChild;
        if (child?.matches('a[href]')) {
          // A label-only link followed by a reason belongs to the same line.
          // A sole link is processed as its own message node.
          if (child===node.lastChild && child===node.firstChild) continue;
          link=child;
        }
      }
      const text=summaryLine(node,box,old,link,links);
      if (!text) continue;
      now.summaryTexts.set(node,text);
      let field, prefixField, hasLabel=false;
      for (const [label,candidate] of labels) {
        if (text.startsWith(label) && /^\s*[:：]\s*\S/u.test(text.slice(label.length))) {
          if (hasLabel) { prefixField=null; break; }
          prefixField=candidate; hasLabel=true;
        }
      }
      if (link) {
        const href=link.getAttribute('href') || '';
        if (!/^#[^#\s]+$/u.test(href) || href.length>257) continue;
        try {
          const target=new URL(href,document.baseURI), current=new URL(document.URL);
          if (target.origin!==current.origin || target.pathname!==current.pathname || target.search!==current.search) continue;
          field=ids.get(decodeURIComponent(target.hash.slice(1)));
        } catch { continue; }
        if (hasLabel && prefixField!==field) continue;
        // A jump link carrying just the destination name supplies no reason.
        if (names.get(field)===text) continue;
      } else {
        field=prefixField;
      }
      if (!field || !safeFields.has(field) || links.has(node.id) ||
          field.hasAttribute('aria-errormessage') ||
          (field.getAttribute('aria-invalid')==='true' && field.hasAttribute('aria-describedby'))) continue;
      now.summaryOwners.set(node,field);
      if (!now.summaryReasons.has(field)) now.summaryReasons.set(field,new Set());
      now.summaryReasons.get(field).add(text);
      if (!old || (old.priorOwner.has(node) && old.priorOwner.get(node)!==field) ||
          old.summaryTexts.get(node)===text || old.visibleText.get(node)===text ||
          old.summaryReasons.get(field)?.has(text) ||
          (old.summaryOwners.has(node) && old.summaryOwners.get(node)!==field)) continue;
      const association=box.matches(summaryMarker) || field.getAttribute('aria-invalid')==='true'?'summary_error':'summary_text';
      if (!pairs.has(field)) pairs.set(field,new Map());
      pairs.get(field).set(node,{node,text,association});
      now.priorOwner.set(node,field);
    }
  }

  // Keep field attribution separate from the single-message query. Weak keys
  // retain the pre-click identities without keeping detached controls alive.
  function fieldSnapshot(record, form, previous, extended=false) {
    try { return collectFields(record, form, previous, extended); }
    catch { return null; } // Named form controls may shadow DOM methods.
  }
  function collectFields(record, form, previous, extended=false) {
    if (!record.isConnected || !form.isConnected || owner(form)!==record) return null;
    const ids=new Map(), links=new Map(), controls=[], counts=new Map(), candidates=[];
    const excluded=new WeakSet(), originals=new WeakMap(), visibleText=new WeakMap();
    const oldByField=new WeakMap(), priorOwner=new WeakMap(), appearances=new WeakMap(), visual=[];
    const eligible=node=>node && owner(node)===record &&
      (!node.closest('form') || node.closest('form')===form);
    const additional=extended ? {existing:new WeakSet(),foreign:new WeakSet(),indicatorFields:new WeakMap(),
      indicatorOwners:new WeakMap(),summaryTexts:new WeakMap(),summaryReasons:new WeakMap(),
      summaryOwners:new WeakMap(),priorOwner} : null;
    const safeFields=new Set(), iconCandidates=[], entries=[], indicators=[];
    const inputSelector='input,select,textarea';
    const messageTag=node=>node.matches('small,span,p,div,output,strong,em,[role="alert"],[role="status"]');
    walk(form.getRootNode(),false,node=>{
      const element=node.nodeType===Node.ELEMENT_NODE ? node : node.parentElement;
      if (!element || !eligible(element)) excluded.add(node);
      if (extended) {
        const box=fieldSummary(element,form,record);
        if (!previous) {
          additional.existing.add(node);
          if (!eligible(element) && !box) additional.foreign.add(node);
        }
        if (node.nodeType===Node.ELEMENT_NODE) {
          if (eligible(node) && node.matches('i,img,svg,span,[role="img"]')) iconCandidates.push(node);
          if (box && (node.matches('li,p,small,a[href]') ||
              ((node.parentElement===box || node===box) && node.matches('span,div')))) {
            const item=node.closest('li');
            // A fragment link in a list belongs to this line only when the
            // line contains that link and ordinary formatting, no other field.
            if (!item || node===item || (node.parentElement===item && node===item.firstChild && node===item.lastChild))
              entries.push([node,box]);
          }
        }
      }
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
    const isSummary=node=>{
      const box=fieldSummary(node,form,record);
      return box && !counts.has(box) && (box!==node || box.matches(summaryMarker+',ul,ol,.form-summary') || counts.get(box.parentElement)!==1);
    };
    function adjacent(field,node,safe,invalid) {
      if (links.has(node.id) || (extended && (isSummary(node) || validationIcon(node)))) return;
      const strong=invalid || node.matches('.field-error,.error,.invalid-feedback,.error-message,[data-error="true"]');
      add(field,node,strong?'adjacent_error':'adjacent_text',safe);
    }
    // Remember visible text before it becomes linked, including a formerly
    // generic container which the page later promotes to an error message.
    if (!previous) for (const node of candidates) textOfMessage(node);
    function add(field,node,association,safe) {
      if (!eligible(node) || excluded.has(node) || previous?.excluded.has(node)) return;
      if (priorOwner.has(node) && priorOwner.get(node)!==field) {
        priorOwner.set(node,null);
      } else if (!priorOwner.has(node)) priorOwner.set(node,field);
      if (!safe || (extended && previous?.summaryOwners.has(node) && previous.summaryOwners.get(node)!==field) ||
          (previous?.priorOwner.has(node) && previous.priorOwner.get(node)!==field)) return;
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
        if (extended) {
          safeFields.add(field);
          indicatorSources(field,form,record,counts,iconCandidates,appearance,previous,additional,indicators);
        }
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
        if (extended && !directForm) {
          for (let msg=part.previousElementSibling; msg; msg=msg.previousElementSibling) {
            if (msg.localName==='label' && msg.control===field) continue;
            if (!messageTag(msg) || counts.has(msg) || canonical.recordKind(msg) || !eligible(msg)) break;
            adjacent(field,msg,safe,invalid);
          }
        }
        if (!directForm || bareOK) {
          for (let msg=part.nextElementSibling; msg && messageTag(msg); msg=msg.nextElementSibling) {
            if (counts.has(msg) || canonical.recordKind(msg) || !eligible(msg)) break;
            adjacent(field,msg,safe,invalid);
          }
        }
        if (directForm) break;
      }
    }
    if (limits.size) return null;
    if (extended && entries.length) summaryFeedback(record,controls,safeFields,entries,ids,links,counts,previous,additional,pairs);
    if (limits.size) return null;
    if (!previous) return {originals,excluded,visibleText,oldByField,priorOwner,appearances,...additional};
    const rows=[];
    for (const [field,byNode] of pairs) {
      const owned=[...byNode.values()].filter(item=>priorOwner.get(item.node)===field);
      const marked=owned.filter(item=>item.association!=='adjacent_text' && item.association!=='summary_text');
      const choices=marked.length?marked:owned;
      if (choices.length!==1) continue;
      const {node,text,association}=choices[0];
      rows.push({field_node:remember(field,true),message_node:remember(node,true),reason:text,association});
      if (rows.length>16) break;
    }
    const output={fields:rows.length<=16?rows:[],visual_changes:visual.length<=16?
      visual.map(({field,changes})=>({field_node:remember(field,true),changes})):[]};
    if (extended && indicators.length<=32) {
      const messages=new Set(output.fields.map(row=>row.message_node));
      output.indicators=indicators.map(({field,source,kind})=>({
        field_node:remember(field,true),source_node:remember(source,true),kind,
      })).filter(row=>!messages.has(row.source_node));
    }
    return output;
  }
  if (operation==='submit_fields_read') {
    const empty={fields:[],visual_changes:[]};
    const baseline=registry.submitFeedback, form=baseline?.form.deref();
    const record=registry.nodes.get(args.record)?.weak.deref();
    if (!baseline || !form || !record || record!==baseline.record.deref() ||
        baseline.anchor!==args.anchor_text || !matchesRecord(record,true) || limits.size) return result(empty);
    remember(record,true);
    const extended=args.include_indicators===true && Boolean(baseline.extended);
    const prior=extended?baseline.extended:baseline.fields;
    return result((prior && fieldSnapshot(record,form,prior,extended)) || empty);
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
    elements=0; texts=0; others=0; limits.clear();
    const extended=fieldSnapshot(record,form,null,true);
    if (snapshot || fields || extended) {
      const baseline={record:new WeakRef(record),form:new WeakRef(form),anchor:args.anchor_text,
        fields,extended,texts:snapshot && new Set(snapshot.found.map(item=>item.text)),rejected:snapshot?.rejected};
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
