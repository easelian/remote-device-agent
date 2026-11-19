// Return only the painted surface host to the native adapter in the isolated realm.
function(x, y) {
  let node = document.elementFromPoint(x, y);
  for (let depth = 0; node && node.shadowRoot; depth++) {
    if (depth >= 8) return null;
    const child = node.shadowRoot.elementFromPoint(x, y);
    if (!child || child === node) break;
    node = child;
  }
  if (!node) return null;
  for (let parent = node; parent; parent = parent.parentElement || parent.getRootNode().host) {
    const tag = parent.localName;
    if (['a', 'button', 'input', 'select', 'textarea', 'label', 'iframe', 'frame'].includes(tag)
        || parent.isContentEditable || parent.inert || parent.getAttribute('aria-disabled') === 'true') return null;
    const hints = ['id','name','autocomplete','type'].map(attr => parent.getAttribute(attr) || '');
    if (hints.some(hint => hint.length > 256 || /password|passcode|secret|token|one-time-code|cc-number|cc-csc|captcha/i.test(hint))) return null;
    const style = getComputedStyle(parent);
    if (Number(style.opacity) === 0 || style.visibility !== 'visible') return null;
  }
  return node;
}
