export function moduleDataUrl(source) {
  return `data:text/javascript;charset=utf-8,${encodeURIComponent(source)}`;
}
