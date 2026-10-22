function controlLabel(control) {
  const labels = control.labels?.length ? Array.from(control.labels) : [control.closest('label')].filter(Boolean);
  const read = node => {
    if (node === control) return '';
    if (node.nodeType === 3) return node.nodeValue;
    if (node.nodeType !== 1) return '';
    const tag = node.localName;
    if (tag === 'script' || tag === 'style' || node.getAttribute('aria-hidden') === 'true') return '';
    const style = node.ownerDocument.defaultView.getComputedStyle(node);
    if (style.display === 'none' || style.visibility === 'hidden') return '';
    if (tag === 'br') return ' ';
    if (tag === 'img') return node.getAttribute('alt') || '';
    const content = Array.from(node.childNodes, read).join('');
    return /^(block|flex|grid|table|list-item|flow-root)/.test(style.display) ? ' ' + content + ' ' : content;
  };
  return labels.map(read).join(' ').replace(/\s+/g, ' ').trim() || null;
}
