"""Generates varied UI pages for training an interactable-element detector.

Each page is one self-contained HTML file. Labels are not written here: rendering the pages with
the ignored `collect_generated_perception_rows` worker test records every visible interactive
element's box from the DOM.

    python3 generate_pages.py --out pages/ --count 1500 --seed 7
"""

import argparse
import html
import pathlib
import random

GLYPHS = ["✉", "⚙", "☰", "✕", "⋮", "⋯", "♥", "★", "⟳", "+", "−", "✎", "⌂", "◀", "▶", "▲", "▼", "⚑", "⎘", "⤴", "⤓", "?", "!", "i"]
# Inline SVG icons: (viewBox content) drawn with currentColor.
SVGS = [
    '<circle cx="11" cy="11" r="7" fill="none" stroke="currentColor" stroke-width="2"/><path d="M16 16l5 5" stroke="currentColor" stroke-width="2"/>',
    '<path d="M4 7h16M4 12h16M4 17h16" stroke="currentColor" stroke-width="2"/>',
    '<path d="M6 6l12 12M18 6L6 18" stroke="currentColor" stroke-width="2"/>',
    '<path d="M12 5v14M5 12h14" stroke="currentColor" stroke-width="2"/>',
    '<path d="M9 6l6 6-6 6" fill="none" stroke="currentColor" stroke-width="2"/>',
    '<path d="M4 20h4l10-10-4-4L4 16z" fill="none" stroke="currentColor" stroke-width="2"/>',
    '<circle cx="12" cy="8" r="4" fill="currentColor"/><path d="M4 21c1-5 15-5 16 0" fill="currentColor"/>',
    '<path d="M12 21s-8-5-8-11a5 5 0 019-3 5 5 0 019 3c0 6-8 11-8 11z" fill="none" stroke="currentColor" stroke-width="2"/>',
    '<path d="M6 9a6 6 0 0112 0v5l2 3H4l2-3z" fill="none" stroke="currentColor" stroke-width="2"/>',
    '<rect x="4" y="5" width="16" height="14" rx="2" fill="none" stroke="currentColor" stroke-width="2"/><path d="M4 7l8 6 8-6" fill="none" stroke="currentColor" stroke-width="2"/>',
    '<path d="M12 4v11M7 10l5 5 5-5M5 20h14" fill="none" stroke="currentColor" stroke-width="2"/>',
    '<circle cx="5" cy="12" r="2" fill="currentColor"/><circle cx="12" cy="12" r="2" fill="currentColor"/><circle cx="19" cy="12" r="2" fill="currentColor"/>',
]
ACTIONS = [
    "Save", "Cancel", "Continue", "Sign in", "Log in", "Next", "Back", "Submit", "Delete", "Edit", "Share",
    "Download", "Upload", "Search", "Apply", "Reset", "Learn more", "Buy now", "Add to cart", "Subscribe",
    "Follow", "Message", "Invite", "Export", "Filter", "Sort", "View all", "Details", "Open", "Close",
    "Confirm", "Retry", "Archive", "Accept", "Decline", "Get started", "Try it free", "Start trial", "Create",
    "Book now", "Checkout", "Join", "Connect", "Install", "Update", "Remove", "Copy link", "Done",
]
SECTIONS = [
    "Home", "Dashboard", "Reports", "Billing", "Team", "Projects", "Pricing", "Docs", "Blog", "About",
    "Contact", "Support", "Settings", "Profile", "Orders", "Customers", "Products", "Analytics", "Inbox",
    "Calendar", "Files", "Integrations", "Security", "Notifications", "Careers", "Community", "Status",
]
FIELDS = [
    "Email", "Password", "Full name", "Company", "Phone", "City", "Message", "Username", "Card number",
    "Address", "Postal code", "Website", "Job title", "Search", "Promo code", "Country", "Date of birth",
]
WORDS = (
    "the a quick team plan update report total balance account order status active pending monthly "
    "annual review shared recent new open closed weekly growth starter scale invoice payment due "
    "member access level project budget export record summary overview note reminder draft"
).split()
BADGES = ["Active", "Paid", "Pending", "Beta", "New", "Draft", "Overdue", "Trial", "Pro", "Archived"]
FONTS = [
    "system-ui, sans-serif", "Georgia, serif", "Helvetica, Arial, sans-serif", "Verdana, sans-serif",
    "'Trebuchet MS', sans-serif", "Menlo, monospace", "'Times New Roman', serif", "'Avenir Next', sans-serif",
]


def esc(text):
    return html.escape(text, quote=True)


class Page:
    """One random page: a theme plus a stack of sections."""

    def __init__(self, rng):
        self.rng = rng
        dark = rng.random() < 0.3
        hue = rng.randrange(360)
        self.bg = f"hsl({hue}, {rng.randrange(0, 25)}%, {rng.randrange(6, 16) if dark else rng.randrange(94, 100)}%)"
        self.fg = f"hsl({hue}, 10%, {rng.randrange(85, 96) if dark else rng.randrange(8, 25)}%)"
        self.surface = f"hsl({hue}, {rng.randrange(0, 20)}%, {rng.randrange(12, 22) if dark else rng.randrange(86, 99)}%)"
        accent_hue = rng.randrange(360)
        self.accent = f"hsl({accent_hue}, {rng.randrange(45, 90)}%, {rng.randrange(38, 58)}%)"
        self.muted = f"hsl({hue}, 8%, {rng.randrange(45, 65)}%)"
        self.radius = rng.choice([0, 2, 4, 6, 8, 12, 16, 999])
        self.font = rng.choice(FONTS)
        self.size = rng.randrange(12, 18)
        self.gap = rng.randrange(6, 24)

    def words(self, low, high):
        return " ".join(self.rng.choice(WORDS) for _ in range(self.rng.randrange(low, high))).capitalize()

    def icon(self):
        if self.rng.random() < 0.6:
            size = self.rng.choice([14, 16, 18, 20, 24])
            return f'<svg width="{size}" height="{size}" viewBox="0 0 24 24" aria-hidden="true">{self.rng.choice(SVGS)}</svg>'
        return f'<span aria-hidden="true">{self.rng.choice(GLYPHS)}</span>'

    def button(self, label=None):
        rng = self.rng
        label = label or rng.choice(ACTIONS)
        style = rng.choice(["filled", "outline", "ghost", "pill", "link"])
        pad_y, pad_x = rng.randrange(4, 14), rng.randrange(8, 24)
        radius = 999 if style == "pill" else self.radius
        if style == "filled":
            colors = f"background:{self.accent};color:#fff;border:none"
        elif style == "outline":
            colors = f"background:transparent;color:{self.accent};border:{rng.choice([1, 2])}px solid {self.accent}"
        elif style == "link":
            colors = f"background:none;border:none;color:{self.accent};text-decoration:underline"
        else:
            colors = f"background:{self.surface};color:{self.fg};border:none"
        inner = esc(label)
        if rng.random() < 0.25:
            inner = f"{self.icon()} {inner}" if rng.random() < 0.5 else f"{inner} {self.icon()}"
        return (
            f'<button type="button" style="{colors};padding:{pad_y}px {pad_x}px;border-radius:{radius}px;'
            f'font:inherit;font-size:{rng.randrange(11, 18)}px;cursor:pointer">{inner}</button>'
        )

    def icon_button(self):
        rng = self.rng
        label = rng.choice(ACTIONS + SECTIONS)
        size = rng.randrange(24, 44)
        background = rng.choice(["transparent", self.surface, self.accent])
        color = "#fff" if background == self.accent else self.fg
        return (
            f'<button type="button" aria-label="{esc(label)}" style="width:{size}px;height:{size}px;'
            f"display:inline-flex;align-items:center;justify-content:center;background:{background};"
            f'color:{color};border:none;border-radius:{rng.choice([self.radius, 999])}px;padding:0">{self.icon()}</button>'
        )

    def link(self, label=None):
        label = label or self.rng.choice(SECTIONS)
        decoration = self.rng.choice(["none", "underline"])
        color = self.rng.choice([self.accent, self.fg, self.muted])
        return f'<a href="#{esc(label.lower())}" style="color:{color};text-decoration:{decoration}">{esc(label)}</a>'

    def field(self):
        rng = self.rng
        name = rng.choice(FIELDS)
        kind = rng.choice(["text", "text", "email", "password", "search", "textarea", "select"])
        border = f"{rng.choice([1, 1, 2])}px solid {self.muted}"
        common = (
            f"font:inherit;padding:{rng.randrange(5, 12)}px;border:{border};border-radius:{self.radius}px;"
            f"background:{self.surface};color:{self.fg};width:{rng.randrange(160, 360)}px"
        )
        ident = f"f{rng.randrange(10**8)}"
        if kind == "textarea":
            control = f'<textarea id="{ident}" rows="{rng.randrange(2, 4)}" style="{common}"></textarea>'
        elif kind == "select":
            options = "".join(f"<option>{esc(rng.choice(SECTIONS))}</option>" for _ in range(3))
            control = f'<select id="{ident}" style="{common}">{options}</select>'
        else:
            placeholder = f' placeholder="{esc(name)}"' if rng.random() < 0.6 else ""
            control = f'<input id="{ident}" type="{kind}"{placeholder} style="{common}">'
        if rng.random() < 0.7:
            return f'<label for="{ident}" style="display:block;margin:4px 0">{esc(name)}</label>{control}'
        return control

    def toggle(self):
        rng = self.rng
        kind = rng.choice(["checkbox", "radio", "switch"])
        text = esc(self.words(2, 5))
        if kind == "switch":
            on = rng.random() < 0.5
            track = self.accent if on else self.muted
            knob = "margin-left:auto" if on else ""
            return (
                f'<span style="display:inline-flex;align-items:center;gap:8px"><span role="switch" tabindex="0" '
                f'aria-checked="{str(on).lower()}" aria-label="{text}" style="display:inline-flex;width:36px;height:20px;'
                f'border-radius:999px;background:{track};padding:2px;box-sizing:border-box"><span style="width:16px;'
                f'height:16px;border-radius:999px;background:#fff;{knob}"></span></span>{text}</span>'
            )
        ident = f"t{rng.randrange(10**8)}"
        return f'<label style="display:inline-flex;gap:6px;align-items:center"><input type="{kind}" id="{ident}" name="g{rng.randrange(4)}"> {text}</label>'

    def badge(self):
        """Pill-shaped text that is not interactive: a negative example."""
        color = self.rng.choice([self.accent, self.muted])
        return (
            f'<span style="border:1px solid {color};color:{color};border-radius:999px;padding:2px 8px;'
            f'font-size:12px">{esc(self.rng.choice(BADGES))}</span>'
        )

    def nav(self):
        rng = self.rng
        links = "".join(f'<span style="margin-right:{self.gap}px">{self.link()}</span>' for _ in range(rng.randrange(2, 6)))
        icons = "".join(self.icon_button() for _ in range(rng.randrange(0, 4)))
        cta = self.button(rng.choice(["Sign in", "Log in", "Get started", "Try it free", "Contact sales"])) if rng.random() < 0.5 else ""
        if rng.random() < 0.3:
            links = (
                f'<input type="search" placeholder="{esc(rng.choice(FIELDS))}" style="font:inherit;padding:8px;'
                f'border:none;border-radius:{self.radius}px;width:{rng.randrange(200, 480)}px">' + links
            )
        return (
            f'<header style="display:flex;align-items:center;gap:{self.gap}px;padding:12px 24px;background:{self.surface}">'
            f'<strong style="font-size:{self.size + 4}px;margin-right:{self.gap * 2}px">{esc(self.words(1, 3))}</strong>'
            f'<nav>{links}</nav><span style="margin-left:auto;display:flex;gap:8px;align-items:center">{icons}{cta}</span></header>'
        )

    def form(self):
        rng = self.rng
        parts = [self.field() for _ in range(rng.randrange(1, 5))]
        parts += [self.toggle() for _ in range(rng.randrange(0, 3))]
        buttons = " ".join(self.button() for _ in range(rng.randrange(1, 3)))
        rows = "".join(f'<div style="margin:{self.gap // 2}px 0">{part}</div>' for part in parts)
        return f"<section><h2>{esc(self.words(1, 4))}</h2>{rows}<div>{buttons}</div></section>"

    def cards(self):
        rng = self.rng
        cards = []
        for _ in range(rng.randrange(2, 5)):
            footer = self.button() if rng.random() < 0.6 else self.link(rng.choice(ACTIONS))
            extra = self.badge() if rng.random() < 0.4 else ""
            cards.append(
                f'<div style="background:{self.surface};border-radius:{self.radius}px;padding:16px;width:{rng.randrange(180, 260)}px">'
                f"<h3 style=\"margin:0 0 8px\">{esc(self.words(1, 3))} {extra}</h3><p>{esc(self.words(6, 14))}</p>{footer}</div>"
            )
        return f'<section style="display:flex;flex-wrap:wrap;gap:{self.gap}px">{"".join(cards)}</section>'

    def table(self):
        rng = self.rng
        select = rng.random() < 0.5
        cell = f"padding:{rng.randrange(6, 14)}px;border-bottom:1px solid {self.muted}"
        head = "".join(
            f'<th style="{cell};text-align:left;font-size:12px">{esc(name)}</th>'
            for name in rng.sample(["Name", "Plan", "Status", "Balance", "Owner", "Updated", "Amount", "Type"], 4)
        )
        head = (f'<th style="{cell}"><input type="checkbox"></th>' if select else "") + head
        rows = []
        for _ in range(rng.randrange(2, 7)):
            action = self.icon_button() if rng.random() < 0.5 else self.link(rng.choice(["Edit", "View", "Open", "Details"]))
            status = self.badge() if rng.random() < 0.6 else esc(rng.choice(BADGES))
            check = f'<td style="{cell}"><input type="checkbox"></td>' if select else ""
            rows.append(
                f'<tr>{check}<td style="{cell}">{esc(self.words(1, 3))}</td><td style="{cell}">{status}</td>'
                f'<td style="{cell}">${rng.randrange(0, 9999)}.{rng.randrange(10, 99)}</td><td style="{cell}">{action}</td></tr>'
            )
        return (
            f'<section><table style="border-collapse:collapse;width:{rng.randrange(60, 100)}%">'
            f'<tr>{head}<th style="{cell}"></th></tr>{"".join(rows)}</table></section>'
        )

    def results(self):
        """A list of result cards: a title link, plain metadata text, sometimes a snippet."""
        rng = self.rng
        items = []
        for _ in range(rng.randrange(2, 6)):
            snippet = f'<p style="margin:4px 0">{esc(self.words(8, 20))}</p>' if rng.random() < 0.5 else ""
            items.append(
                f'<div style="border:1px solid {self.muted};border-radius:{self.radius}px;padding:{rng.randrange(10, 20)}px;margin:8px 0">'
                f'{self.link(self.words(2, 6))}<div style="color:{self.muted};font-size:13px;margin-top:6px">{esc(self.words(2, 4))}</div>{snippet}</div>'
            )
        return f'<section><div style="color:{self.muted}">{esc(self.words(2, 4))}</div>{"".join(items)}</section>'

    def filters(self):
        """Bare native checkboxes and radios with text, as in a filter column."""
        rng = self.rng
        kind = rng.choice(["checkbox", "radio"])
        items = "".join(
            f'<label style="display:block;margin:{rng.randrange(4, 12)}px 0"><input type="{kind}" name="filter"> {esc(self.words(2, 4))}</label>'
            for _ in range(rng.randrange(2, 6))
        )
        return f'<section><div style="font-size:12px;color:{self.muted}">{esc(rng.choice(SECTIONS).upper())}</div>{items}</section>'

    def toolbar(self):
        rng = self.rng
        items = [self.icon_button() for _ in range(rng.randrange(2, 7))]
        if rng.random() < 0.5:
            items.insert(rng.randrange(len(items) + 1), self.field())
        return f'<section style="display:flex;gap:{rng.randrange(2, 12)}px;align-items:center;flex-wrap:wrap">{"".join(items)}</section>'

    def tabs(self):
        rng = self.rng
        names = rng.sample(SECTIONS, rng.randrange(2, 6))
        current = rng.randrange(len(names))
        tabs = "".join(
            f'<button role="tab" type="button" aria-selected="{str(index == current).lower()}" style="background:none;border:none;'
            f'border-bottom:2px solid {self.accent if index == current else "transparent"};padding:8px 12px;color:{self.fg};font:inherit">{esc(name)}</button>'
            for index, name in enumerate(names)
        )
        return f'<section><div role="tablist" style="display:flex;gap:4px">{tabs}</div><p>{esc(self.words(8, 20))}</p></section>'

    def banner(self):
        buttons = " ".join(self.button() for _ in range(self.rng.randrange(1, 3)))
        return (
            f'<section style="background:{self.surface};border-left:4px solid {self.accent};padding:12px 16px;'
            f'display:flex;gap:12px;align-items:center"><span>{esc(self.words(5, 12))}</span>{buttons}{self.icon_button()}</section>'
        )

    def prose(self):
        return f"<section><h2>{esc(self.words(2, 5))}</h2><p>{esc(self.words(20, 50))}.</p></section>"

    def pager(self):
        pages = "".join(self.link(str(number)) + " " for number in range(1, self.rng.randrange(3, 8)))
        return f"<section>{self.button('Back')} {pages}{self.button('Next')}</section>"

    def render(self):
        rng = self.rng
        builders = [
            self.form, self.cards, self.table, self.toolbar, self.tabs, self.banner, self.prose, self.pager,
            self.results, self.filters,
        ]
        sections = "".join(
            f'<div style="margin:{self.gap}px 0">{rng.choice(builders)()}</div>' for _ in range(rng.randrange(2, 5))
        )
        sidebar = ""
        if rng.random() < 0.35:
            items = "".join(
                f'<div style="margin:8px 0">{self.icon() if rng.random() < 0.5 else ""} {self.link()}</div>'
                for _ in range(rng.randrange(3, 8))
            )
            sidebar = f'<aside style="width:{rng.randrange(160, 240)}px;background:{self.surface};padding:16px">{items}</aside>'
        return (
            f'<!doctype html><html><head><meta charset="utf-8"><title>{esc(self.words(1, 3))}</title></head>'
            f'<body style="margin:0;background:{self.bg};color:{self.fg};font-family:{self.font};font-size:{self.size}px">'
            f"{self.nav() if rng.random() < 0.85 else ''}"
            f'<div style="display:flex">{sidebar}<main style="flex:1;padding:{rng.randrange(12, 40)}px">{sections}</main></div>'
            "</body></html>"
        )


def generate(count, seed):
    """`count` pages, the same for the same seed."""
    rng = random.Random(seed)
    return [Page(random.Random(rng.randrange(2**32))).render() for _ in range(count)]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, type=pathlib.Path)
    parser.add_argument("--count", type=int, default=1500)
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    for index, page in enumerate(generate(args.count, args.seed)):
        (args.out / f"page-{index:05d}.html").write_text(page)
    print(f"wrote {args.count} pages to {args.out}")


if __name__ == "__main__":
    main()
