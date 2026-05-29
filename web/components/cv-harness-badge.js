// <cv-harness-badge harness="claude"> — a small colored pill naming the harness.
import { esc, HARNESS_LABELS } from "./util.js";

class CvHarnessBadge extends HTMLElement {
  static get observedAttributes() { return ["harness"]; }
  attributeChangedCallback() { this.render(); }
  connectedCallback() { this.render(); }

  render() {
    const h = (this.getAttribute("harness") || "").toLowerCase();
    const label = HARNESS_LABELS[h] || h || "?";
    this.className = "cv-badge";
    this.dataset.harness = h;
    this.textContent = label;
    this.setAttribute("title", `harness: ${label}`);
  }
}

customElements.define("cv-harness-badge", CvHarnessBadge);
