# Thamus

TODO: add some kind of pretentious and incomprehensible word soup here

![obligatory slop banner](./slop_banner.webp)

Because, I swear, nobody else is putting in **any** effort!

## Features

- Low resource usage 📉
- - I heard of people having dedicated machines to run what is basically... just a proxy between an interface and a model API? I laugh. That is nonsense 🙄
- Memory/save system 📝
- - Includes automatically spewing memories into context (infusions) 💉
- Hot config reloading 🔥
- - All files are watched and updates read out in real time 👀
- - All config changes are respected **live**, even halfway through a response ⏺️
- The bot can edit its own config 🐍
- - This is gated by a whitelist of what can be edited 🛂
- - Yeah, you can totally whitelist it to edit the whitelist, but please don't do that 🙏
- **_No_** prompt cache invalidation (outside of intentional compactions/summarization) 🥳
- - No, like _seriously_, there are **no** invalidations that can happen otherwise 🎉
- - _Even if you change the prompt_, it is added as a **diff** until the next compaction 🪩
- **Extremely** low context window overhead 🤏
- - Seriously, it is tiny compared to others
- - **All** strings injected into context besides chat messages, are **fully configurable**
- Background processing / dreams 😴
- - Ensmallens the last summary, manages memories, and makes new compactions 🧹
- - This means conversations can go forever without ever waiting for compaction ♾️
- Abstract platform implementations 🔧
- - Includes voice support 🗣️
- - Has live/partial output streaming 📡
- Handles media/attachments 📷
- MCP support 💩
- Usable built-in tools 🧰
- Everything needed to function on its own as an "agent" 🤖
- Works in group chats with actual usable behavior 👥
- **Everything** is configurable (mostly) 🛠️
- Automatic slot management for working efficiently with llama.cpp 🦙
- Le funny image, voice, and song generation 🏭
- At least moderately decent internal architecture that can be perceived and extended 🧑‍💻
- - **No** incomprehensible "smoking gun gate is green worth noting load bearing" garbage 😄
- - You can even read the code. I even wrote it. I even wrote all these stupid emojis 🤣

## I had to build this

You should all feel sorry.

All I wanted, was a simple chatbot. It should talk to multiple people, have some basic things like memories, not perform like **garbage**, have almost **no** configuration or interaction, use all the tools you would ever want, **not** turn your computer into a _gaping security hole_ or _require you putting your app tokens inside a sandbox_, use APIs correctly, **properly** handle attachments/media in a _good_ way, and the rest be implemented with some _general common sense_.

### WALL OF SHAME

I at least _tried_ to _try_ these out. I was disgusted 🤮. I cannot bare to look at these for a funny joke bot let alone seriousness.

- Like 3 or 4 things ending in "Claw"
- Sticking chatty things onto Clod code and more minimal agents
- The accountant from Futurama
- Various other slop piles, of varying popularity

None of them passed the smell test, half of them didn't work, absolutely none of it was reasonable. It makes me sick to think about these. I can't use them, that's why I had to make this.
