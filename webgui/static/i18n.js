"use strict";
// Only interface messages are translated. Repository data and native byte/text
// output never enter this registry, even when they equal an interface label.
const i18n = (() => {
  const texts = new WeakMap(), attributes = new WeakMap();
  const storageKey = "wasm-git-language";
  let language;
  try { language = localStorage.getItem(storageKey); } catch { /* Storage may be disabled. */ }
  if (!["eu_US", "zh_CN"].includes(language)) {
    const preferred = (navigator.languages || [navigator.language]).find(value => /^(zh|en)(-|$)/i.test(value)) || "en";
    language = /^zh/i.test(preferred) ? "zh_CN" : "eu_US";
  }
  class Message {
    constructor(key, values = {}) { this.key = key; this.values = values; }
    toString() {
      const source = language === "zh_CN" ? zhCN[this.key] ?? this.key : this.key;
      return source.replace(/\{(\w+)\}/g, (match, key) => Object.hasOwn(this.values, key) ? String(this.values[key]) : match);
    }
  }
  function setText(node, value) {
    const text = document.createTextNode(String(value));
    node.replaceChildren(text);
    if (value instanceof Message) texts.set(text, value);
    return node;
  }
  // Capture the static HTML once, before client data or editable forms arrive.
  function capture() {
    const walker = document.createTreeWalker(document.documentElement, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
    while (walker.nextNode()) {
      const node = walker.currentNode;
      if (node.nodeType === Node.TEXT_NODE) {
        if (node.parentElement.closest("script,style,textarea,pre,code")) continue;
        const key = node.data.trim();
        if (Object.hasOwn(zhCN, key)) {
          const before = node.data.slice(0, node.data.indexOf(key)), after = node.data.slice(node.data.indexOf(key) + key.length);
          texts.set(node, {toString: () => before + new Message(key) + after});
        }
      } else {
        const keys = {};
        for (const name of ["title", "placeholder", "aria-label"]) {
          const key = node.getAttribute(name);
          if (Object.hasOwn(zhCN, key)) keys[name] = new Message(key);
        }
        if (Object.keys(keys).length) attributes.set(node, keys);
      }
    }
  }
  function apply() {
    document.documentElement.lang = language === "zh_CN" ? "zh-CN" : "en-US";
    const walker = document.createTreeWalker(document.documentElement, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
    while (walker.nextNode()) {
      const node = walker.currentNode;
      if (texts.has(node)) node.data = String(texts.get(node));
      for (const [name, value] of Object.entries(attributes.get(node) || {})) node.setAttribute(name, String(value));
    }
    document.getElementById("language").value = language;
  }
  function setLanguage(value) {
    if (!["eu_US", "zh_CN"].includes(value)) return;
    language = value;
    try { localStorage.setItem(storageKey, value); } catch { /* The current page still switches. */ }
    apply();
    document.dispatchEvent(new Event("languagechange"));
  }
  function error(value) {
    if (value instanceof Message) return value;
    if (Object.hasOwn(zhCN, value)) return new Message(value);
    return new Message("The operation could not finish. Original diagnostic: {detail}", {detail: value});
  }
  capture(); apply();
  document.getElementById("language").onchange = event => setLanguage(event.target.value);
  return {message: (key, values) => key instanceof Message ? key : new Message(key, values), setText, error, setLanguage,
    translate: (key, values) => String(new Message(key, values)), has: key => Object.hasOwn(zhCN, key)};
})();
const tr = i18n.message;
const setText = i18n.setText;
