// Populate the sidebar
//
// This is a script, and not included directly in the page, to control the total size of the book.
// The TOC contains an entry for each page, so if each page includes a copy of the TOC,
// the total size of the page becomes O(n**2).
class MDBookSidebarScrollbox extends HTMLElement {
    constructor() {
        super();
    }
    connectedCallback() {
        this.innerHTML = '<ol class="chapter"><li class="chapter-item affix "><a href="introduction.html">Introduction</a></li><li class="chapter-item affix "><li class="part-title">Getting Started</li><li class="chapter-item "><a href="getting-started/installation.html"><strong aria-hidden="true">1.</strong> Installation</a></li><li class="chapter-item "><a href="getting-started/quick-start-python.html"><strong aria-hidden="true">2.</strong> Quick Start (Python)</a></li><li class="chapter-item "><a href="getting-started/quick-start-julia.html"><strong aria-hidden="true">3.</strong> Quick Start (Julia)</a></li><li class="chapter-item affix "><li class="part-title">Documentation</li><li class="chapter-item "><a href="explanation/index.html"><strong aria-hidden="true">4.</strong> Explanation</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="explanation/architecture.html"><strong aria-hidden="true">4.1.</strong> Architecture</a></li><li class="chapter-item "><a href="explanation/design-choices.html"><strong aria-hidden="true">4.2.</strong> Design Choices</a></li><li class="chapter-item "><a href="explanation/data-model.html"><strong aria-hidden="true">4.3.</strong> Data Model</a></li><li class="chapter-item "><a href="explanation/storage-model.html"><strong aria-hidden="true">4.4.</strong> Storage Model</a></li><li class="chapter-item "><a href="explanation/content-addressing.html"><strong aria-hidden="true">4.5.</strong> Content Addressing</a></li><li class="chapter-item "><a href="explanation/bindings.html"><strong aria-hidden="true">4.6.</strong> Language Bindings</a></li></ol></li><li class="chapter-item "><a href="guides/index.html"><strong aria-hidden="true">5.</strong> Developer Guides</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="guides/rust.html"><strong aria-hidden="true">5.1.</strong> Rust</a></li><li class="chapter-item "><a href="guides/python.html"><strong aria-hidden="true">5.2.</strong> Python</a></li><li class="chapter-item "><a href="guides/julia.html"><strong aria-hidden="true">5.3.</strong> Julia</a></li><li class="chapter-item "><a href="guides/server.html"><strong aria-hidden="true">5.4.</strong> gRPC Server &amp; Client</a></li><li class="chapter-item "><a href="guides/benchmarks.html"><strong aria-hidden="true">5.5.</strong> Benchmarks</a></li></ol></li><li class="chapter-item "><a href="how-to/index.html"><strong aria-hidden="true">6.</strong> How-To Guides</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="how-to/install.html"><strong aria-hidden="true">6.1.</strong> Install the Native Library</a></li><li class="chapter-item "><a href="how-to/integrate-python.html"><strong aria-hidden="true">6.2.</strong> Integrate with Python</a></li><li class="chapter-item "><a href="how-to/integrate-julia.html"><strong aria-hidden="true">6.3.</strong> Integrate with Julia</a></li><li class="chapter-item "><a href="how-to/run-server.html"><strong aria-hidden="true">6.4.</strong> Run the gRPC Server</a></li><li class="chapter-item "><a href="how-to/use-cli.html"><strong aria-hidden="true">6.5.</strong> Use the cas CLI</a></li></ol></li><li class="chapter-item "><a href="reference/index.html"><strong aria-hidden="true">7.</strong> Reference</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="reference/file-format.html"><strong aria-hidden="true">7.1.</strong> On-Disk File Format</a></li><li class="chapter-item "><a href="reference/rust-api.html"><strong aria-hidden="true">7.2.</strong> Rust API</a></li><li class="chapter-item "><a href="reference/python-api.html"><strong aria-hidden="true">7.3.</strong> Python API</a></li><li class="chapter-item "><a href="reference/julia-api.html"><strong aria-hidden="true">7.4.</strong> Julia API</a></li><li class="chapter-item "><a href="reference/c-abi.html"><strong aria-hidden="true">7.5.</strong> C ABI</a></li><li class="chapter-item "><a href="reference/grpc-api.html"><strong aria-hidden="true">7.6.</strong> gRPC API</a></li><li class="chapter-item "><a href="reference/server-config.html"><strong aria-hidden="true">7.7.</strong> Server Configuration</a></li><li class="chapter-item "><a href="reference/cli.html"><strong aria-hidden="true">7.8.</strong> CLI</a></li></ol></li><li class="chapter-item "><li class="spacer"></li><li class="chapter-item affix "><a href="contributing.html">Contributing</a></li></ol>';
        // Set the current, active page, and reveal it if it's hidden
        let current_page = document.location.href.toString().split("#")[0].split("?")[0];
        if (current_page.endsWith("/")) {
            current_page += "index.html";
        }
        var links = Array.prototype.slice.call(this.querySelectorAll("a"));
        var l = links.length;
        for (var i = 0; i < l; ++i) {
            var link = links[i];
            var href = link.getAttribute("href");
            if (href && !href.startsWith("#") && !/^(?:[a-z+]+:)?\/\//.test(href)) {
                link.href = path_to_root + href;
            }
            // The "index" page is supposed to alias the first chapter in the book.
            if (link.href === current_page || (i === 0 && path_to_root === "" && current_page.endsWith("/index.html"))) {
                link.classList.add("active");
                var parent = link.parentElement;
                if (parent && parent.classList.contains("chapter-item")) {
                    parent.classList.add("expanded");
                }
                while (parent) {
                    if (parent.tagName === "LI" && parent.previousElementSibling) {
                        if (parent.previousElementSibling.classList.contains("chapter-item")) {
                            parent.previousElementSibling.classList.add("expanded");
                        }
                    }
                    parent = parent.parentElement;
                }
            }
        }
        // Track and set sidebar scroll position
        this.addEventListener('click', function(e) {
            if (e.target.tagName === 'A') {
                sessionStorage.setItem('sidebar-scroll', this.scrollTop);
            }
        }, { passive: true });
        var sidebarScrollTop = sessionStorage.getItem('sidebar-scroll');
        sessionStorage.removeItem('sidebar-scroll');
        if (sidebarScrollTop) {
            // preserve sidebar scroll position when navigating via links within sidebar
            this.scrollTop = sidebarScrollTop;
        } else {
            // scroll sidebar to current active section when navigating via "next/previous chapter" buttons
            var activeSection = document.querySelector('#sidebar .active');
            if (activeSection) {
                activeSection.scrollIntoView({ block: 'center' });
            }
        }
        // Toggle buttons
        var sidebarAnchorToggles = document.querySelectorAll('#sidebar a.toggle');
        function toggleSection(ev) {
            ev.currentTarget.parentElement.classList.toggle('expanded');
        }
        Array.from(sidebarAnchorToggles).forEach(function (el) {
            el.addEventListener('click', toggleSection);
        });
    }
}
window.customElements.define("mdbook-sidebar-scrollbox", MDBookSidebarScrollbox);
