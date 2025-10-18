# SRC-D_WEYU Dashboard

**By MΛVΞRΙCK / Evanscira Labs**

I’m MΛVΞRΙCK, a futurist tinkerer blending tech, creativity, and survival skills into tools that work. **SRC-D_WEYU** is a dashboard for organizing projects, prompts, media, and AI workflows, featuring backups, merging, and collaborative imports.

This project is functional but still evolving, and I need support to refine, expand, and maintain it. If you value tools that help creators, researchers, and innovators stay organized and productive, your contribution matters.

**Ways to support:**

- Star ⭐ the repo to help visibility
- Share with peers who could benefit
- Financial support via [VENMO @MavArtCreator](https://venmo.com/MavArtCreator) to accelerate development

Every bit of support brings SRC-D_WEYU closer to being the robust, all-in-one productivity tool it’s meant to be.

Hey! 👋

I’ve been building a little tool to help people streamline their workflows without losing progress — kind of like a lightweight dashboard for organizing and saving stuff locally.

👉 Check it out: https://evansciralabs.github.io/SRC-D_WEYU/

Right now, it works great for organizing webpage versions (or prompts, notes, recipes, etc.) as “projects.” It also has a scratchpad that can save whatever you write — and you can then attach that content to a project.
For example: one project could hold multiple recipes, ideas, or prompt drafts, all nested neatly in one place.

⚠️ Heads-up:
It uses localStorage and sessionStorage, so your data resets if you clear cache or end the session — but you can back up or share your entire dashboard file for safekeeping or collaboration.

If you create a project, write something in the scratchpad, and add it to that project, you can always recall it directly from the dashboard — no more relying on the AI chat history to remember context.

Would love any feedback. I’ve got tons of ideas to make it better!


---

✅ Security / Privacy

Fully client-side — no tracking, network calls, or API requests (only Tailwind + Google Fonts).

Data is stored only in localStorage, sessionStorage, and IndexedDB on your own device.

No uploads or remote storage — totally local.


✅ Functionality

Create, edit, and delete projects/incidents.

Scratchpad autosaves to sessionStorage and can preview HTML.

Export/import .srcd JSON files for easy backup or merging dashboards.

Attach files (images, PDFs, etc.) via IndexedDB, label or delete them freely.

Fully responsive layout (desktop + mobile FAB menu).

Custom modals — no blocking alert() or confirm() calls.


✅ Warnings

Data clears if cache/session is reset.

Local-only for now (no cloud sync yet).


✅ Visual / UX

“Cyber-console” aesthetic with Space Mono + Michroma fonts.

Smooth transitions and clear panel organization.

Scanline overlay gives it a subtle matrix-style polish.
