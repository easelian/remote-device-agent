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
  const remember = node => {
    for (const [id, entry] of registry.nodes) {
      if (entry.weak.deref() === node) return id;
    }
    if (registry.nodes.size >= 128) registry.nodes.delete(registry.nodes.keys().next().value);
    const id = ++registry.serial;
    if (!Number.isSafeInteger(id)) throw new Error('Evidence identity exhausted');
    registry.nodes.set(id,{weak:new WeakRef(node),created:now});
    return id;
  };
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
  function matchesRecord(record) {
    if (canonical.recordKind(record) !== 'record') return false;
    for (let item = record, depth = 0; item; item = parent(item), depth++) {
      if (depth >= 64) { limits.add('depth_limit'); return false; }
      if (!visit(item) || !allowsRecordText(item)) return false;
    }
    const stack = [{next:record.firstChild}];
    let visited = 0, characters = 0, anchor = false, expected = false;
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
          anchor ||= text === args.anchor_text;
          expected ||= text === args.expected_text;
        }
      }
    }
    return anchor && expected;
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
