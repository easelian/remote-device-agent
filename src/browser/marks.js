(() => {
  // What marks an element as a control on the page itself: its tag and its attributes. The
  // names a browser gives to roles differ from one version to the next, so they decide nothing.
  const roles = new Set(['button', 'link', 'textbox', 'searchbox', 'combobox', 'checkbox', 'radio', 'spinbutton', 'slider',
    'switch', 'menuitem', 'tab', 'option', 'gridcell', 'treeitem', 'menuitemcheckbox', 'menuitemradio']);
  // The first role of a control that the role attribute names, or an empty string.
  const role = node => (node.getAttribute('role') || '').toLowerCase().split(/\s+/).find(name => roles.has(name)) || '';
  // A control that HTML defines, or the outermost element of an editable region.
  const native = node => {
    switch (node.tagName) {
      case 'BUTTON': case 'TEXTAREA': case 'SELECT': case 'OPTION': return true;
      case 'A': return node.hasAttribute('href');
      case 'INPUT': return node.type !== 'hidden';
      case 'SUMMARY': return node.parentElement?.tagName === 'DETAILS';
    }
    return node.isContentEditable === true && !node.parentElement?.isContentEditable;
  };
  // Whether the markup alone marks an element: as a control, or as one that takes clicks or
  // keys through a handler or a place in the focus order.
  const marked = node => native(node) || role(node) !== '' || node.hasAttribute('onclick')
    || (node.hasAttribute('tabindex') && node.tabIndex >= 0);
  // A record: a table row, a list item, an article or a dialog. A reference is bound to the
  // text of its record, and what a search finds is returned with its record.
  const record = node => /^(TR|LI|ARTICLE|DIALOG)$/.test(node.tagName)
    || /^(row|listitem|article|treeitem|option|dialog|alertdialog)$/.test(node.getAttribute('role') || '');
  return {role, native, marked, record};
})()
