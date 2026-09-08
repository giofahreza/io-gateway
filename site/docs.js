const legacyHashRoutes = {
  "feature-index": "/docs/",
  "quick-start": "/docs/quick-start/",
  "first-client-request": "/docs/first-client-request/",
  configuration: "/docs/configuration/",
  dashboard: "/docs/dashboard/",
  "provider-accounts": "/docs/provider-accounts/",
  "routing-models": "/docs/routing-and-models/",
  "custom-models": "/docs/custom-models/",
  "test-api": "/docs/test-api/",
  "api-keys": "/docs/api-keys/",
  notifications: "/docs/notifications/",
  "usage-quota": "/docs/usage-and-quota/",
  deployment: "/docs/deployment/",
  troubleshooting: "/docs/troubleshooting/",
};

const article = document.querySelector(".docs-article");
const outline = document.getElementById("on-this-page");
const searchInput = document.getElementById("docs-search");
const searchResults = document.getElementById("docs-search-results");
const docsNavigation = document.querySelector(".docs-toc");
const docsNavigationToggle = document.querySelector(".docs-nav-toggle");

function redirectLegacyHashRoute() {
  const hash = decodeURIComponent(window.location.hash.slice(1));
  const cleanPath = legacyHashRoutes[hash];

  if (cleanPath && window.location.pathname.replace(/index\.html$/, "") === "/docs/") {
    window.location.replace(cleanPath);
  }
}

function slugify(value) {
  return value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

function installActivePageState() {
  const currentPath = normalizePath(window.location.pathname);

  document.querySelectorAll(".docs-toc a[href]").forEach((link) => {
    const linkPath = normalizePath(new URL(link.href, window.location.origin).pathname);
    const isActive = linkPath === currentPath;
    link.classList.toggle("is-active", isActive);

    if (isActive) {
      link.setAttribute("aria-current", "page");
    } else {
      link.removeAttribute("aria-current");
    }
  });
}

function normalizePath(pathname) {
  const clean = pathname.replace(/index\.html$/, "");
  return clean.endsWith("/") ? clean : clean + "/";
}

function buildOutline() {
  if (!article || !outline) {
    return;
  }

  const headings = [...article.querySelectorAll("h2, h3")].filter(
    (heading) => !heading.closest(".docs-grid, .docs-related"),
  );
  const usedIds = new Set([...document.querySelectorAll("[id]")].map((element) => element.id));
  const fragment = document.createDocumentFragment();

  headings.forEach((heading) => {
    if (!heading.id) {
      const base = slugify(heading.textContent.trim()) || "section";
      let id = base;
      let suffix = 2;

      while (usedIds.has(id)) {
        id = base + "-" + suffix;
        suffix += 1;
      }

      heading.id = id;
      usedIds.add(id);
    }

    const link = document.createElement("a");
    link.href = "#" + heading.id;
    link.textContent = heading.textContent.trim();
    link.dataset.depth = heading.tagName === "H3" ? "3" : "2";
    fragment.appendChild(link);
  });

  if (fragment.childNodes.length === 0) {
    outline.parentElement.hidden = true;
    return;
  }

  outline.replaceChildren(fragment);
}

function installBreadcrumbs() {
  if (!article || article.querySelector(".docs-breadcrumbs")) {
    return;
  }

  const title = article.querySelector("h1")?.textContent.trim() || "Routebook";
  const slug = article.dataset.pageSlug || "";
  const breadcrumbs = document.createElement("nav");
  breadcrumbs.className = "docs-breadcrumbs";
  breadcrumbs.setAttribute("aria-label", "Breadcrumb");

  const home = document.createElement("a");
  home.href = "/";
  home.textContent = "IO Gateway";

  const docs = document.createElement("a");
  docs.href = "/docs/";
  docs.textContent = "Routebook";

  breadcrumbs.append(home, divider(), docs);

  if (slug) {
    const current = document.createElement("span");
    current.setAttribute("aria-current", "page");
    current.textContent = title;
    breadcrumbs.append(divider(), current);
  } else {
    docs.setAttribute("aria-current", "page");
  }
  article.prepend(breadcrumbs);
}

function divider() {
  const slash = document.createElement("span");
  slash.setAttribute("aria-hidden", "true");
  slash.textContent = "/";
  return slash;
}

function installMobileOutline() {
  if (!article || !outline || article.querySelector(".docs-mobile-outline")) {
    return;
  }

  const links = [...outline.querySelectorAll("a")];

  if (!links.length) {
    return;
  }

  const details = document.createElement("details");
  details.className = "docs-mobile-outline";

  const summary = document.createElement("summary");
  summary.textContent = "Contents";

  const nav = document.createElement("nav");
  links.forEach((link) => {
    const clone = link.cloneNode(true);
    nav.appendChild(clone);
  });

  details.append(summary, nav);

  const lead = article.querySelector(".docs-lead");
  if (lead) {
    lead.after(details);
  } else {
    article.querySelector(".docs-heading")?.after(details);
  }
}

function installDocsNavigation() {
  if (!docsNavigation || !docsNavigationToggle) {
    return;
  }

  const compactViewport = window.matchMedia("(max-width: 860px)");
  const stateMark = docsNavigationToggle.querySelector("[aria-hidden='true']");

  function setExpanded(expanded) {
    docsNavigation.classList.toggle("is-expanded", expanded);
    docsNavigationToggle.setAttribute("aria-expanded", String(expanded));
    if (stateMark) stateMark.textContent = expanded ? "−" : "+";
  }

  function syncForViewport() {
    setExpanded(!compactViewport.matches);
  }

  docsNavigationToggle.addEventListener("click", () => {
    setExpanded(!docsNavigation.classList.contains("is-expanded"));
  });

  document.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || !compactViewport.matches || !docsNavigation.classList.contains("is-expanded")) {
      return;
    }

    setExpanded(false);
    docsNavigationToggle.focus();
  });

  if (compactViewport.addEventListener) {
    compactViewport.addEventListener("change", syncForViewport);
  } else if (compactViewport.addListener) {
    compactViewport.addListener(syncForViewport);
  }

  syncForViewport();
}

function installHeadingSpy() {
  const links = [...document.querySelectorAll(".docs-on-page a")];

  if (!links.length) {
    return;
  }

  const linksById = new Map(links.map((link) => [decodeURIComponent(link.hash.slice(1)), link]));

  function setActiveLink(activeLink) {
    links.forEach((link) => {
      const isActive = link === activeLink;
      link.classList.toggle("is-active", isActive);

      if (isActive) {
        link.setAttribute("aria-current", "location");
      } else {
        link.removeAttribute("aria-current");
      }
    });
  }

  setActiveLink(links[0]);

  if (!("IntersectionObserver" in window)) {
    return;
  }

  const observer = new IntersectionObserver(
    (entries) => {
      const visible = entries
        .filter((entry) => entry.isIntersecting)
        .sort((left, right) => left.boundingClientRect.top - right.boundingClientRect.top);
      const active = visible[0]?.target?.id;

      if (!active) {
        return;
      }

      setActiveLink(linksById.get(active));
    },
    {
      rootMargin: "-84px 0px -72% 0px",
      threshold: 0.01,
    },
  );

  linksById.forEach((_link, id) => {
    const heading = document.getElementById(id);
    if (heading) {
      observer.observe(heading);
    }
  });
}

function installCodeBlocks() {
  document.querySelectorAll(".docs-article pre").forEach((pre) => {
    if (pre.parentElement?.classList.contains("docs-code-block")) {
      return;
    }

    const frame = document.createElement("div");
    frame.className = "docs-code-block";

    const toolbar = document.createElement("div");
    toolbar.className = "docs-code-toolbar";

    const label = document.createElement("span");
    label.className = "docs-code-label";
    label.textContent = pre.textContent.trimStart().startsWith("{") ? "JSON example" : "Terminal example";

    const button = document.createElement("button");
    button.className = "docs-code-copy";
    button.type = "button";
    button.textContent = "Copy";
    button.setAttribute("aria-label", "Copy code example");

    const status = document.createElement("span");
    status.className = "sr-only";
    status.setAttribute("aria-live", "polite");

    pre.tabIndex = 0;
    pre.setAttribute("aria-label", "Code example. Scroll horizontally to read the full line.");

    button.addEventListener("click", async () => {
      const originalLabel = button.textContent;

      try {
        await copyText(pre.textContent);
        button.textContent = "Copied";
        status.textContent = "Code copied to clipboard.";
      } catch {
        button.textContent = "Copy failed";
        status.textContent = "Unable to copy code to the clipboard.";
      }

      window.setTimeout(() => {
        button.textContent = originalLabel;
        status.textContent = "";
      }, 1800);
    });

    toolbar.append(label, button, status);
    pre.before(frame);
    frame.append(toolbar, pre);
  });
}

async function copyText(value) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value);
    return;
  }

  const helper = document.createElement("textarea");
  helper.value = value;
  helper.setAttribute("readonly", "");
  helper.style.position = "fixed";
  helper.style.opacity = "0";
  document.body.appendChild(helper);
  helper.select();
  const copied = document.execCommand("copy");
  helper.remove();

  if (!copied) {
    throw new Error("clipboard unavailable");
  }
}

function installTableScrollHints() {
  const tables = [...document.querySelectorAll(".docs-table-wrap")].map((wrapper) => {
    wrapper.tabIndex = 0;
    wrapper.setAttribute("role", "region");
    wrapper.setAttribute("aria-label", "Documentation table. Scroll horizontally to read all columns.");

    const hint = document.createElement("p");
    hint.className = "docs-scroll-hint";
    hint.textContent = "Scroll horizontally to read all columns →";
    wrapper.prepend(hint);

    return { wrapper, hint };
  });

  if (!tables.length) {
    return;
  }

  const update = () => {
    tables.forEach(({ wrapper, hint }) => {
      const scrollable = wrapper.scrollWidth > wrapper.clientWidth + 1;
      wrapper.classList.toggle("is-scrollable", scrollable);
      hint.hidden = !scrollable;
    });
  };

  update();

  if ("ResizeObserver" in window) {
    const observer = new ResizeObserver(update);
    tables.forEach(({ wrapper }) => observer.observe(wrapper));
  } else {
    window.addEventListener("resize", update, { passive: true });
  }
}

function installSearch() {
  if (!searchInput || !searchResults) {
    return;
  }

  let pageLinks = buildFallbackSearchPages();

  fetch("/docs/search-index.json", { cache: "no-store" })
    .then((response) => (response.ok ? response.json() : Promise.reject(new Error("search index unavailable"))))
    .then((index) => {
      if (Array.isArray(index.pages) && index.pages.length) {
        pageLinks = index.pages.map((page) => ({
          title: page.title || "",
          category: page.category || "",
          summary: page.summary || "",
          keywords: page.keywords || "",
          headings: Array.isArray(page.headings) ? page.headings.join(" ") : "",
          excerpt: page.excerpt || "",
          href: page.href || "/docs/",
        }));
      }
    })
    .catch(() => {
      pageLinks = buildFallbackSearchPages();
    });

  const updateResults = () => {
    const query = searchInput.value.trim().toLowerCase();

    if (!query) {
      searchResults.classList.remove("is-visible");
      searchResults.replaceChildren();
      return;
    }

    const matches = pageLinks
      .map((page) => {
        const title = page.title.toLowerCase();
        const category = page.category.toLowerCase();
        const summary = page.summary.toLowerCase();
        const keywords = page.keywords.toLowerCase();
        const headings = (page.headings || "").toLowerCase();
        const excerpt = (page.excerpt || "").toLowerCase();
        const haystack = [title, category, summary, keywords, headings, excerpt].join(" ");
        let score = 0;

        if (title.includes(query)) score += 6;
        if (category.includes(query)) score += 3;
        if (headings.includes(query)) score += 3;
        if (keywords.includes(query)) score += 2;
        if (summary.includes(query)) score += 2;
        if (excerpt.includes(query)) score += 1;
        if (!haystack.includes(query)) score = 0;

        return { ...page, score };
      })
      .filter((page) => page.score > 0)
      .sort((left, right) => right.score - left.score || left.title.localeCompare(right.title))
      .slice(0, 10);

    searchResults.replaceChildren();
    searchResults.classList.add("is-visible");

    if (matches.length === 0) {
      const empty = document.createElement("p");
      empty.className = "docs-search-empty";
      empty.textContent = "No docs pages match that search.";
      searchResults.appendChild(empty);
      return;
    }

    matches.forEach((page) => {
      const link = document.createElement("a");
      link.href = page.href;

      const title = document.createElement("strong");
      title.textContent = page.title;

      const summary = document.createElement("small");
      summary.textContent = [page.category, page.summary].filter(Boolean).join(" - ");

      link.append(title, summary);
      searchResults.appendChild(link);
    });
  };

  function clearSearch() {
    searchInput.value = "";
    updateResults();
  }

  searchInput.addEventListener("input", updateResults);

  searchInput.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") {
      return;
    }

    clearSearch();
    searchInput.blur();
  });

  document.addEventListener("keydown", (event) => {
    const target = event.target;
    const typingInControl = target instanceof Element && target.closest("input, textarea, select, [contenteditable='true']");

    if (event.key !== "/" || event.altKey || event.ctrlKey || event.metaKey || typingInControl) {
      return;
    }

    event.preventDefault();

    if (docsNavigation && docsNavigationToggle && !docsNavigation.classList.contains("is-expanded")) {
      docsNavigationToggle.click();
    }

    window.requestAnimationFrame(() => searchInput.focus());
  });
}

function buildFallbackSearchPages() {
  return [...document.querySelectorAll(".docs-toc a[data-title]")].map((link) => ({
    title: link.dataset.title || link.textContent.trim(),
    category: link.dataset.category || "",
    summary: link.dataset.summary || "",
    keywords: link.dataset.keywords || "",
    headings: "",
    excerpt: "",
    href: link.href,
  }));
}

redirectLegacyHashRoute();
installActivePageState();
installBreadcrumbs();
buildOutline();
installMobileOutline();
installDocsNavigation();
installHeadingSpy();
installCodeBlocks();
installTableScrollHints();
installSearch();
