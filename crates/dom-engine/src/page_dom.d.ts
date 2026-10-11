export const SHADOW_HOP: " >>> ";

export function composedQuery(root: ParentNode, selector: string): Element | null;

export function composedQueryAll(root: ParentNode, selector: string): Element[];

export function renderedLeafVisible(el: Element): boolean;

export function isElementHidden(
  element: Element,
  budget?: { remaining: number },
): boolean;

export function isRenderedHidden(element: Element): boolean;

export function modalDialogOpen(document: Document): boolean;
