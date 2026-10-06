<p align="center">
  <img src="icons/icon-app.png" width="112" alt="Логотип SNATCH">
</p>

<h1 align="center">SNATCH</h1>

<p align="center">
  <strong>Лёгкий загрузчик видео, музыки, файлов и торрентов.<br>yt-dlp + aria2c, нативное окно и удобный терминал.</strong>
</p>

<p align="center">
  <a href="https://github.com/RERCON0/SNATCH/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/RERCON0/SNATCH/actions/workflows/ci.yml/badge.svg?branch=main"></a>
  <a href="https://github.com/RERCON0/SNATCH/actions/workflows/security.yml"><img alt="Security" src="https://github.com/RERCON0/SNATCH/actions/workflows/security.yml/badge.svg?branch=main"></a>
  <a href="https://github.com/RERCON0/SNATCH/releases/latest"><img alt="Последний релиз" src="https://img.shields.io/github/v/release/RERCON0/SNATCH?color=8b5cf6"></a>
  <a href="https://github.com/RERCON0/SNATCH/releases"><img alt="Загрузки" src="https://img.shields.io/github/downloads/RERCON0/SNATCH/total?color=0ea5e9"></a>
  <a href="LICENSE"><img alt="GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-2f855a"></a>
  <a href="https://t.me/rercon"><img alt="Telegram" src="https://img.shields.io/badge/Telegram-@rercon-26A5E4?logo=telegram&logoColor=white"></a>
</p>

<p align="center">
  <a href="https://github.com/RERCON0/SNATCH/releases/latest"><strong>Скачать для Windows</strong></a> ·
  <a href="#использование">Быстрый старт</a> ·
  <a href="#подпись-и-проверка-релиза">Проверить подпись</a> ·
  <a href="docs/RELEASING.md">Сборка релиза</a> ·
  <a href="SECURITY.md">Безопасность</a>
</p>

![SNATCH — нативный GUI](docs/screenshot.png)

Вставьте ссылку, выберите качество и папку — SNATCH подберёт загрузчик,
покажет прогресс и запомнит историю. Видео и стримы скачивает yt-dlp;
прямые ссылки, torrent и magnet — aria2c.

Лёгкость — одна из главных идей SNATCH: Rust, нативный интерфейс без Electron
и портативный запуск. Загрузчики устанавливаются отдельно, когда нужны.

> [!NOTE]
> CLI `snatch` и GUI `snatch-app` используют один движок и общие настройки.
> Можно чередовать окно и терминал: история ссылок и папок остаётся общей.

## Возможности

- **Автовыбор движка** для видео, прямых ссылок и торрентов.
- **Качество на выбор**: до 2160p, mp3 или исходное аудио, субтитры и плейлисты.
- **Докачка после сбоя** с проверкой принадлежности файла исходной загрузке.
- **Несколько задач в CLI**: до трёх одновременно по умолчанию, `-j 1..16`.
- **История** последних 15 ссылок и папок, тёмная и светлая темы GUI.
- **Установка инструментов из приложения**: yt-dlp, aria2c, ffmpeg и Deno.

## CLI или GUI

| | `snatch` | `snatch-app` |
|---|---|---|
| Интерфейс | Терминал с подсказками | Нативное окно |
| Без вопросов | `-y` и аргументы | Поля и кнопки |
| Папка | Меню, диски или `-o` | Встроенный проводник |
| Прогресс | Индикаторы каждой задачи | Полоса прогресса и журнал |

## Установка

Скачайте Windows x64 ZIP из [Releases](https://github.com/RERCON0/SNATCH/releases), распакуйте и запустите
`snatch-app.exe` или `snatch.exe`. Устанавливать сам SNATCH не нужно;
CLI можно добавить в `PATH`.

> [!IMPORTANT]
> EXE пока не имеют Authenticode-подписи, поэтому Windows SmartScreen может
> показать «Система Windows защитила ваш компьютер». Если архив скачан из
> [официального Releases](https://github.com/RERCON0/SNATCH/releases) и вы доверяете
> этой сборке, нажмите **«Подробнее» → «Выполнить в любом случае»**.
> Подпись Ed25519 подтверждает пакет, но не убирает это предупреждение Windows.
> Если кнопка запуска недоступна, не меняйте политики защиты ради установки.

> [!TIP]
> Загрузчики можно поставить из GUI кнопкой установки/обновления
> или командой `snatch --install-tools`. Они сохраняются в
> `%LOCALAPPDATA%\snatch\bin`; менять `PATH` не требуется.

ffmpeg нужен для объединения видео и звука, mp3 и субтитров; Deno —
для JavaScript-проверок YouTube. Оба устанавливаются вместе с загрузчиками.

<details>
<summary>Ручная установка через WinGet</summary>

```powershell
winget install yt-dlp.yt-dlp
winget install aria2.aria2
winget install Gyan.FFmpeg
winget install DenoLand.Deno
```

Инструменты ищутся в папке SNATCH, затем в `PATH` и каталогах WinGet.
Свои пути задаются через `SNATCH_YT_DLP` и `SNATCH_ARIA2C` —
[пример](docs/CLI.md#пути-к-внешним-загрузчикам).

</details>

## Использование

В GUI вставьте ссылку или перетащите локальный `.torrent`, выберите
движок, формат и папку, затем нажмите «Скачать». Журнал открывается
в отдельном окне; всплывающие окна закрываются кликом снаружи.

CLI можно запустить без аргументов или сразу со ссылкой:

```powershell
snatch
snatch "https://youtube.com/watch?v=..."
snatch "magnet:?xt=urn:btih:..."
```

Несколько ссылок разделяйте пробелом; пути к торрентам с пробелами
заключайте в кавычки. Пустой ввод открывает историю. Для очереди папка
выбирается один раз, движок и формат — отдельно для каждой ссылки.

> [!TIP]
> В выборе папки CLI доступны отдельные пункты «Диск C:\», «Диск D:\»
> и другие подключённые диски. Выберите диск, затем нужную папку.
> Путь можно передать сразу: `-o "D:\Downloads"`.

Режим без вопросов:

```powershell
snatch "https://host/file.zip" -y -o "D:\Downloads"
snatch "https://youtube.com/watch?v=..." -y -o "D:\Video" -f 1080p
snatch "https://youtube.com/watch?v=..." -y -o "D:\Music" -f audio
snatch "magnet:?xt=urn:btih:..." "https://host/file.zip" -y -o "D:\Downloads" -j 2
```

| Аргумент | Назначение |
|---|---|
| `-o, --output` | Папка сохранения |
| `-j, --jobs` | Число одновременных задач: 1–16 |
| `-e, --engine` | Выбрать движок вручную |
| `-f, --format` | `best`, 2160p–480p, `audio` (mp3), `audio-src` |
| `--subs`, `--playlist` | Субтитры или весь плейлист |
| `--cookies-from-browser` | Сессия браузера для авторизации |
| `--no-continue` | Повторить прерванную загрузку с начала |
| `--clear-history` | Очистить историю ссылок и папок |

Полный список, дополнительные аргументы и настройка путей —
[в справке CLI](docs/CLI.md) и `snatch --help`.

### Как выбирается движок

| Ссылка | Движок |
|---|---|
| `magnet:…`, локальные `.torrent` и `.metalink` | aria2c |
| Прямые ссылки на файлы: `.mp4`, `.zip`, `.iso`, `.pdf` и другие | aria2c |
| Страницы видео, плейлисты и стримы | yt-dlp |

Если для прямой ссылки выбран yt-dlp, он может использовать aria2c как
внешний загрузчик. Докачка в этом пути отключена для защиты существующих файлов.

### Если YouTube просит войти

Войдите в YouTube в браузере и передайте его сессию:

```powershell
snatch "https://youtube.com/watch?v=..." -y -o "D:\Video" --cookies-from-browser chrome
```

При ошибке авторизации интерактивный CLI предложит повтор с куками;
в GUI появится кнопка повтора. Можно указать профиль: `chrome:Profile 1`.

> [!IMPORTANT]
> Куки дают доступ к аккаунту. SNATCH передаёт их локальному yt-dlp;
> используйте браузерную сессию только на доверенной машине.
> Если база cookies заблокирована браузером, закройте его и повторите.

### Докачка и существующие файлы

SNATCH продолжает собственные прерванные загрузки и защищает чужие файлы-тёзки.

> [!CAUTION]
> Если файлы торрента уже существуют, а состояние `.aria2` потеряно,
> выберите пустую папку для новой загрузки. Для сохранения скачанных частей
> сделайте копию и проверьте их торрент-клиентом перед перезаписью.

## Настройки и безопасность

Настройки находятся в `%LOCALAPPDATA%\snatch\config.json` на Windows
и `~/.config/snatch/config.json` на Linux/macOS. История ограничена 15 записями;
`snatch --clear-history` очищает её без удаления остальных настроек.

Дочерние процессы запускаются без shell, автоустановка проверяет контрольные
суммы и размеры файлов, конфиг сохраняется под блокировкой. Подробные границы
доверия, докачка и работа с учётными данными описаны в [SECURITY.md](SECURITY.md).

## Подпись и проверка релиза

Новый процесс сборки, начиная с исходников **0.5.3**, создаёт ZIP с обоими EXE,
лицензиями и подписанным манифестом **Ed25519**. Манифест связывает хеши файлов
с точным Git-коммитом, деревом исходников, Cargo.lock и версией компилятора.
Подпись проверяется до распаковки; изменённые, лишние и повторяющиеся файлы
отклоняются. Архивы старых релизов не имеют этой подписи.

> [!IMPORTANT]
> Доверенный ключ берите из репозитория, а не только из проверяемого ZIP.

Получите доверенный [публичный ключ](release/public-key.pem) и проверяющий
[скрипт](scripts/release.py) из репозитория. В корне checkout выполните:

```powershell
python scripts/release.py verify .\snatch-windows-x64.zip
```

Проверке нужны **Python 3.13+** и **OpenSSL 3** (в Windows есть в Git for Windows).
Встроенный в ZIP ключ сравнивается с внешним доверенным ключом; замена обоих
файлов внутри архива не делает поддельную подпись действительной.
Отпечаток публичного ключа SHA-256 (DER SubjectPublicKeyInfo):

```text
22f55367a7a9bf635237c2da1e50e4c89338733d7fea5ab616615d75fc620afc
```

Это подпись релизного пакета. Authenticode-подпись EXE этим процессом не заявляется.
Подробности — в [инструкции сборки](docs/RELEASING.md).

## Проверки CI

| Проверка | Что контролирует |
|---|---|
| [CI](https://github.com/RERCON0/SNATCH/actions/workflows/ci.yml) | Windows и Linux: форматирование, Clippy без предупреждений, тесты CLI/GUI/движка и проверки подмены ZIP |
| Windows release build | Архитектура x64, правильные CLI/GUI-подсистемы, ASLR, DEP и отсутствие зависимости от VC++ Redistributable |
| [Security](https://github.com/RERCON0/SNATCH/actions/workflows/security.yml) | RustSec ежедневно и при изменениях; уязвимости, unsound и отозванные версии блокируют проверку; секреты ищутся во всей Git-истории |
| [Dependency watch](https://github.com/RERCON0/SNATCH/actions/workflows/dependency-watch.yml) | Новая стабильная версия Rust и изменение закреплённого релиза aria2 требуют ревью |
| [Dependabot](.github/dependabot.yml) | Еженедельные предложения обновить Cargo.lock и закреплённые Actions |

Все Actions закреплены полными SHA. CI публикует **неподписанные кандидаты**
для диагностики; релизная подпись создаётся отдельно, приватный ключ хранится
вне Git и не передаётся runner'ам. Предупреждения о неподдерживаемых зависимостях
остаются видны: состояние `ttf-parser` и план обновления описаны в [SECURITY.md](SECURITY.md).


## Разработка

Установите Rust через Rustup; для Windows также нужны MSVC Build Tools.
Версия компилятора закреплена в `rust-toolchain.toml`.

```powershell
cargo build --locked
cargo run --locked --bin snatch
cargo run --locked --bin snatch-app
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Обычная сборка EXE: `cargo build --locked --release --bins`.
Подписанный пакет с происхождением сборки создаётся по
[инструкции выпуска](docs/RELEASING.md).

> [!NOTE]
> `cargo run` пересобирает изменённые исходники перед запуском.
> Отдельная копия, установленная на `PATH`, обновляется командой
> `cargo install --path . --bins --force --locked`.

## Лицензия

[GNU GPL v3.0 или новее](LICENSE). Лицензия встроенного шрифта —
[уведомление Cascadia Mono / SIL OFL](fonts/OFL-notice.txt).

[Telegram автора](https://t.me/rercon) · [Исходники](https://github.com/RERCON0/SNATCH)
