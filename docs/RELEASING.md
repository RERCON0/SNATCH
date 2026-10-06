# Подписанный релиз SNATCH

Релизный пакет содержит только `snatch.exe`, `snatch-app.exe`, README, GPL,
уведомление о лицензии шрифта, `manifest.json`, `manifest.sig` и публичный ключ.
Приватный ключ в архив не попадает. Проверка ZIP не извлекает и не запускает EXE.

## Подготовка

Нужны Windows x64, MSVC Build Tools, Rustup, Python 3.13+ и OpenSSL 3.
Rust 1.99.0 закреплён в `rust-toolchain.toml`; Cargo.lock обязателен.
Сначала выполните проверки из корня репозитория:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python -m unittest discover -s scripts -p "test_*.py" -v
cargo audit --deny yanked --deny unsound
```

Для audit установите `cargo install cargo-audit --version 0.22.2 --locked`.
Закоммитьте изменения и убедитесь, что CI и Security проходят на этом коммите.

## Ключ издателя

У SNATCH собственный Ed25519-ключ, отдельный от других проектов.
Доверенный публичный ключ — [`release/public-key.pem`](../release/public-key.pem).
Приватный ключ владельца проекта хранится в
`%LOCALAPPDATA%\SNATCH\release-signing\snatch-private.pem`, вне рабочего дерева.
Каталог ограничен ACL текущего пользователя. Резервную копию приватного ключа
нужно хранить отдельно; его потеря потребует публично объявить смену ключа.

Для своего форка создайте отдельную пару, предварительно задав новый путь
публичного ключа. Команда не перезаписывает существующие ключи:

```powershell
python scripts/release.py keygen --private-key "$env:LOCALAPPDATA\MyFork\release-signing\fork-private.pem" --public-key .\release\fork-public.pem
```

Перед сборкой своего форка замените доверенный `release/public-key.pem` новым
публичным ключом и опубликуйте его отпечаток в документации форка.

## Сборка

```powershell
python scripts/release.py build --private-key "$env:LOCALAPPDATA\SNATCH\release-signing\snatch-private.pem"
python scripts/release.py verify .\dist\snatch-windows-x64.zip
Get-FileHash .\dist\snatch-windows-x64.zip -Algorithm SHA256
```

Сборщик требует чистый закоммиченный checkout и запрещает переменные,
подменяющие компилятор, rustflags или release profile. Он использует новое
временное дерево `target`, запускает `cargo build --locked --release --bins`
для MSVC x64 и проверяет PE: архитектуру, CLI/GUI-подсистемы, ASLR, DEP и CRT.
После компиляции все входные файлы и Git-ревизия проверяются повторно.
Это фиксация происхождения сборки; побитовая воспроизводимость разных машин
не заявляется.

Манифест содержит версию продукта, commit/tree Git, SHA-256 всех отслеживаемых
исходных файлов как агрегированный отпечаток, хеш Cargo.lock, версии Cargo/Rust,
размеры и SHA-256 полезной нагрузки, свойства PE и отпечаток публичного ключа.
Ed25519 подписывает точные канонические байты манифеста. Новый ZIP проверяется
тем же независимым путём верификации перед атомарной заменой прежнего архива.

CI использует временные тестовые ключи для положительных и отрицательных
проверок. Они не доверяются издателем и не могут подписать официальный релиз.
Артефакты CI явно названы unsigned; публикация GitHub Release выполняется
отдельно после проверки подписанного ZIP. Вместе с релизом укажите commit и
SHA-256 архива; исходный код этого commit доступен по URL из манифеста.

## Проверка скачанного ZIP

Используйте ключ и проверяющий скрипт из доверенного checkout SNATCH:

```powershell
python scripts/release.py verify C:\Downloads\snatch-windows-x64.zip
```

Скрипт отклоняет неправильную подпись/ключ, несовпадение хешей, неизвестные
поля манифеста, лишние/пропущенные/повторные ZIP-члены, симлинки, пути вне
допустимого списка, слишком большие данные и усечённые архивы. Валидация
происходит без распаковки. Доверенный ключ нельзя брать только из самого ZIP.

Подпись пакета подтверждает издателя и целостность файлов. Это не сертификат
Authenticode и не замена локальным настройкам доверия Windows.
