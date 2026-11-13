@echo off
rem Builds Shelf-Setup.exe on a PC that has Rust and Inno Setup 6 installed.
cargo build --release -p shelf || exit /b 1
"C:\Program Files (x86)\Inno Setup 6\ISCC.exe" installer\shelf.iss || exit /b 1
echo.
echo Done: installer\Output\Shelf-Setup.exe
