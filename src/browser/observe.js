function(maxNodes, maxScan) {
  // The isolated world keeps only this observation's bounded node handles.
  // TreeWalker does not materialize a wide node's complete child array.
  const nodes = [], summary = [], omitted = new Set();
  let scanned = 0;
  const interactive = new Set(['A', 'BUTTON', 'INPUT', 'TEXTAREA', 'SELECT', 'OPTION', 'SUMMARY']);
  const excluded = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'HEAD']);
  const filter = {acceptNode(node) {
    return node.nodeType === 1 && excluded.has(node.tagName) ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT;
  }};
  const walkers = [document.createTreeWalker(document, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, filter)];
  while (walkers.length && scanned < maxScan && summary.length < maxNodes) {
    const node = walkers[walkers.length - 1].nextNode();
    if (!node) { walkers.pop(); continue; }
    scanned++;
    if (node.nodeType === 3) {
      const name = node.nodeValue.slice(0, 512).trim();
      const parent = node.parentElement;
      if (!name || !parent || parent.closest('button,a,select,textarea,script,style,h1,h2,h3,h4,h5,h6')) continue;
      const style = getComputedStyle(parent);
      if (style.display === 'none' || style.visibility === 'hidden' || !parent.getClientRects().length) continue;
      summary.push({role: 'StaticText', name});
      if (node.nodeValue.length > 512) omitted.add('text_clipped');
      continue;
    }
    if (node.shadowRoot) {
      if (walkers.length < 64) walkers.push(document.createTreeWalker(node.shadowRoot, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT, filter));
      else omitted.add('shadow_depth_limit');
    } else if (node.tagName.includes('-')) {
      omitted.add('custom_element_internals_unverified');
    }
    if (!interactive.has(node.tagName) && !/^H[1-6]$/.test(node.tagName) && !node.hasAttribute('role') && !node.isContentEditable) continue;
    // Avoid asking native AX to serialize an individually huge control.
    let attributeBytes = 0;
    for (const attribute of node.attributes) {
      attributeBytes += attribute.name.length + attribute.value.length;
      if (attributeBytes > 8192) break;
    }
    if (attributeBytes > 8192 || node.childNodes.length > 1024) { omitted.add('oversized_control'); continue; }
    const index = nodes.length;
    nodes.push(node);
    summary.push({index});
  }
  if (walkers.length) omitted.add('observation_budget');
  return {nodes, summary, scanned, omitted: [...omitted]};
}
