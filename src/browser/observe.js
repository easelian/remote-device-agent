function(marks, options) {
  // One bounded reader in this isolated world. A fresh observation closes the old one.
  globalThis.__rdhBrowserReader?.close();
  // Elements with these tags are shown to the browser even where the page does not make them
  // controls: the browser knows of click handlers that the markup does not show.
  const tags = new Set(['A', 'BUTTON', 'INPUT', 'TEXTAREA', 'SELECT', 'OPTION', 'SUMMARY']);
  // Controls that are named by what they contain, by tag and by role.
  const wordedTags = new Set(['A', 'BUTTON', 'SELECT', 'TEXTAREA', 'SUMMARY']);
  const wordedRoles = new Set(['button', 'link', 'checkbox', 'radio', 'switch', 'menuitem', 'tab', 'option', 'gridcell', 'treeitem',
    'menuitemcheckbox', 'menuitemradio']);
  const excluded = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD']);
  const filter = {acceptNode(node) {
    return node.nodeType === 1 && excluded.has(node.tagName) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT;
  }};
  // The page may change while it is read, and the read goes on. Time is counted in batches:
  // the page can only change between two of them. `reached` holds the batch in which the walk
  // came to a node, and `altered` the batch after which the text or children under a node last
  // changed.
  let batch = 0;
  const reached = new WeakMap(), altered = new WeakMap();
  // `trail` is the way from the root of a walk down to where it stopped: each node with its
  // place among its siblings.
  const walk = root => {
    reached.set(root, batch);
    return {walker: document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, filter), trail: []};
  };
  const state = {pending: [], walkers: [walk(document)], text: null, changed: false, closed: false, omitted: new Set(), roots: []};
  const note = records => {
    for (const record of records) {
      // A change to what the walk has not come to yet is simply read later.
      if (reached.has(record.target)) state.changed = true;
      if (record.type === 'attributes') continue;
      for (let node = record.target; node && altered.get(node) !== batch; node = node.parentNode || node.host) altered.set(node, batch);
    }
  };
  const edited = event => {if (reached.has(event.target)) state.changed = true;};
  const observer = new MutationObserver(note);
  const watch = root => {
    observer.observe(root, {subtree: true, childList: true, attributes: true, characterData: true});
    root.addEventListener('input', edited, true);
    root.addEventListener('change', edited, true);
  };
  watch(document);
  // Elements whose name holds their text, which is therefore not reported a second time, and
  // the controls among them.
  const worded = new WeakSet(), sealed = new WeakSet();
  const within = (node, set) => {
    for (; node; node = node.parentElement || node.getRootNode().host) if (set.has(node)) return true;
    return false;
  };
  const heading = node => /^H[1-6]$/.test(node.tagName);
  // Whether a node lies in the part of the page that shows now, wholly or in part. A read
  // that is not limited to that part takes every node.
  const range = document.createRange();
  const shows = node => {
    if (!options.viewport) return true;
    if (node.nodeType === 3) range.selectNodeContents(node);
    const box = (node.nodeType === 3 ? range : node).getBoundingClientRect();
    return box.bottom > 0 && box.right > 0 && box.top < innerHeight && box.left < innerWidth && (box.width > 0 || box.height > 0);
  };
  // Whether a name can hold all that an element shows: a little text, and nothing below it
  // that is reported on its own account.
  const small = node => {
    if (node.shadowRoot) return false;
    const walker = document.createTreeWalker(node, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
    let text = '';
    for (let count = 0, inner; (inner = walker.nextNode()); count++) {
      if (count >= 48) return false;
      if (inner.nodeType === 1) {
        if (inner.shadowRoot || tags.has(inner.tagName) || heading(inner) || inner.hasAttribute('role') || marks.marked(inner)) return false;
        continue;
      }
      const words = inner.nodeValue.replace(/\s+/g, ' ').trim();
      if (words) text += (text ? ' ' : '') + words;
      if (text.length > 80) return false;
    }
    return true;
  };
  // Whether an element only wraps controls: it holds some, and no text outside them.
  const wraps = node => {
    let held = false;
    const walker = document.createTreeWalker(node, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, {acceptNode(inner) {
      if (inner.nodeType === 3 || !marks.marked(inner)) return NodeFilter.FILTER_ACCEPT;
      held = true;
      return NodeFilter.FILTER_REJECT;
    }});
    for (let count = 0, inner; (inner = walker.nextNode()); count++) {
      if (count >= 256 || (inner.nodeType === 3 && inner.nodeValue.trim())) return false;
    }
    return held;
  };
  // Whether the pointer turns into a hand over an element. Asked once for each element in a
  // batch, since the answer is wanted for an element and again for its children.
  const hands = new Map();
  const hand = node => {
    let known = hands.get(node);
    if (known === undefined) hands.set(node, known = getComputedStyle(node).cursor === 'pointer');
    return known;
  };
  // An element that neither its tag nor its role makes a control, and that looks like one all
  // the same: its markup holds a click handler, the pointer turns into a hand over it and not
  // over what is around it, or it is in the focus order.
  const inferred = node => {
    if (node === document.body || node === document.documentElement || node.ownerSVGElement) return false;
    const parent = node.parentElement || node.getRootNode().host;
    let looks = node.hasAttribute('onclick') || (hand(node) && !(parent && hand(parent)));
    const style = getComputedStyle(node);
    // An element that has a role, or that scrolls, is in the focus order for another reason.
    if (!looks && node.hasAttribute('tabindex') && node.tabIndex >= 0 && !node.hasAttribute('role')) {
      looks = !/auto|scroll/.test(style.overflowX + style.overflowY);
    }
    if (!looks || style.visibility === 'hidden' || !node.getClientRects().length) return false;
    // Nothing inside a control that is named by what it contains is a control of its own.
    return !within(parent, sealed) && !wraps(node);
  };
  // The last node under `node` in document order.
  const last = node => {
    while (node.lastChild) node = node.lastChild;
    return node;
  };
  const stop = walker => {
    const trail = [];
    for (let node = walker.currentNode; node !== walker.root; node = node.parentNode) {
      trail.unshift({node, next: node.nextSibling, index: Array.prototype.indexOf.call(node.parentNode.childNodes, node)});
    }
    return trail;
  };
  // A walk stays where it stopped while each node on its trail is in place: still a child of
  // the node above, and still ahead of the sibling that followed it. Where the page replaced or
  // moved part of the trail, the walk goes on with that sibling. Where the sibling is gone too,
  // it goes on from the same place among the children that are there now, and what stands in
  // that place counts as read. Either way a walk always moves on, however often the page
  // replaces what it reads.
  const resume = ({walker, trail}) => {
    let at = walker.root, depth = 0;
    const placed = ({node, next}) => node.parentNode === at
      && !(next && next.parentNode === at && (node.compareDocumentPosition(next) & Node.DOCUMENT_POSITION_PRECEDING));
    while (depth < trail.length && placed(trail[depth])) at = trail[depth++].node;
    if (depth === trail.length) return;
    state.changed = true;
    const {next} = trail[depth];
    if (next && next.parentNode === at) {
      if (next.previousSibling) at = last(next.previousSibling);
    } else {
      for (; depth < trail.length; depth++) {
        const child = at.childNodes[trail[depth].index];
        // Fewer children than before: everything under this node counts as read.
        if (!child) {at = last(at); break;}
        at = child;
      }
    }
    for (let element = at.nodeType === 1 ? at : at.parentElement; element; element = element.parentElement) {
      if (excluded.has(element.tagName)) at = last(element);
    }
    walker.currentNode = at;
  };
  state.close = function() {
    observer.disconnect();
    for (const root of [document, ...this.roots]) {
      root.removeEventListener('input', edited, true);
      root.removeEventListener('change', edited, true);
    }
    this.roots = []; this.pending = []; this.walkers = []; this.text = null; this.closed = true;
  };
  state.status = function() {return {changed: this.changed, closed: this.closed, more: !!(this.pending.length || this.text || this.walkers.length)};};
  // Whether a record, such as a table row, shows what it showed when the walk came to it. A
  // reference is bound to the text of its record, so that must be the text that was reported.
  state.steady = function(record) {
    const last = altered.get(record);
    return last === undefined || last < reached.get(record);
  };
  state.collect = function(maxNodes, maxScan) {
    let scanned = 0;
    if (this.closed) return {closed: true};
    batch++;
    hands.clear();
    // What was read and not yet returned is dropped once the page no longer holds it.
    const held = this.pending.filter(entry => (entry.node || entry.source).isConnected);
    if (held.length !== this.pending.length) {this.pending = held; this.changed = true;}
    if (!this.pending.length && (this.walkers.length || this.text)) {
      if (this.text && !this.text.node.isConnected) {this.text = null; this.changed = true;}
      const rooted = this.walkers.filter(entry => entry.walker.root.isConnected);
      if (rooted.length !== this.walkers.length) {this.walkers = rooted; this.changed = true;}
      this.walkers.forEach(resume);
      while ((this.walkers.length || this.text) && scanned < maxScan && this.pending.length < maxNodes) {
        if (this.text) {
          // The text is read as it was when the walk came to it, however many segments it takes.
          const {node, value, offset, preserve} = this.text;
          let end = Math.min(value.length, offset + 512);
          if (end < value.length && value.charCodeAt(end - 1) >= 0xd800 && value.charCodeAt(end - 1) <= 0xdbff) end--;
          const name = value.slice(offset, end);
          this.text = end < value.length ? {node, value, offset: end, preserve} : null;
          if (name.trim() || preserve) this.pending.push({role: 'StaticText', name, text_continued: offset > 0, text_remaining: !!this.text, source: node});
          continue;
        }
        const entry = this.walkers[this.walkers.length - 1], node = entry.walker.nextNode();
        if (!node) {this.walkers.pop(); continue;}
        entry.trail = null;
        scanned++;
        // A node that the page moved to a later place is not read twice.
        if (reached.has(node)) continue;
        reached.set(node, batch);
        if (node.nodeType === 3) {
          const parent = node.parentElement;
          if (!parent || options.interactive || within(parent, worded)) continue;
          const style = getComputedStyle(parent);
          if (style.display === 'none' || style.visibility === 'hidden' || !parent.getClientRects().length || !shows(node)) continue;
          const value = node.nodeValue;
          this.text = {node, value, offset: 0, preserve: value.length > 512};
          continue;
        }
        if (node.shadowRoot) {
          if (this.walkers.length < 64 && this.roots.length < 128) {
            watch(node.shadowRoot);
            this.roots.push(node.shadowRoot);
            this.walkers.push(walk(node.shadowRoot));
          }
          else this.omitted.add('shadow_scope_limit');
        } else if (node.tagName.includes('-')) this.omitted.add('custom_element_internals_unverified');
        // The page says which elements are controls, and of which kind.
        const kind = marks.native(node) ? 'native' : marks.role(node) ? 'aria' : inferred(node) ? 'inferred' : null;
        if (kind === 'native' ? wordedTags.has(node.tagName) : kind === 'aria' ? wordedRoles.has(marks.role(node)) : kind && small(node)) {
          worded.add(node);
          sealed.add(node);
        } else if (heading(node)) worded.add(node);
        // An element that is not a control is still asked about where it is a heading,
        // where it has one of the tags above, or, unless only controls and headings are
        // wanted, where it has a role.
        const role = (node.getAttribute('role') || '').toLowerCase();
        const titled = heading(node) || /(^|\s)heading(\s|$)/.test(role);
        if (!kind && !titled && !(tags.has(node.tagName) && node.type !== 'hidden') && (options.interactive || !role)) continue;
        if (!shows(node)) continue;
        let attributeBytes = 0;
        for (const attribute of node.attributes) {attributeBytes += attribute.name.length + attribute.value.length; if (attributeBytes > 8192) break;}
        if (attributeBytes > 8192 || node.childNodes.length > 1024) {this.omitted.add('oversized_control'); continue;}
        this.pending.push({node, kind});
      }
      // An empty batch can still have unvisited DOM nodes after the scan budget.
      for (const entry of this.walkers) entry.trail ||= stop(entry.walker);
    }
    return {scanned, changed: this.changed, omitted: [...this.omitted], nodes: this.pending.slice(0, maxNodes).map((entry, index) => entry.node ? {index, kind: entry.kind} :
      {role: entry.role, name: entry.name, text_continued: entry.text_continued, text_remaining: entry.text_remaining})};
  };
  state.ack = function(count, chars) {
    this.pending.splice(0, count);
    if (chars > 0 && this.pending[0] && !this.pending[0].node) {
      this.pending[0].name = Array.from(this.pending[0].name).slice(chars).join('');
      this.pending[0].text_continued = true;
    }
    return this.status();
  };
  globalThis.__rdhBrowserReader = state;
  return state;
}
