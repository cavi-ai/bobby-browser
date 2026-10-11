export const SHADOW_HOP: " >>> ";

export function composedQuery(root: ParentNode, selector: string): Element | null;

export function composedQueryAll(root: ParentNode, selector: string): Element[];

export type VisibilityMode = "rendered" | "a11y" | "hit-ready";

export function isHidden(
  element: Element,
  mode: VisibilityMode,
  budget?: { remaining: number },
): boolean;

export function hidden(
  mode: VisibilityMode,
): (element: Element, budget?: { remaining: number }) => boolean;

export function modalDialogOpen(document: Document): boolean;

export function obscuredByOpenModal(document: Document, renderedMatch: unknown): boolean;
