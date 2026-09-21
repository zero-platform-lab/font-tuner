# PSScriptAnalyzer の設定。check-lint.ps1 が読む。
# 除外したルールは、このリポジトリの性質に合わないもの。理由を必ず書く。
@{
    ExcludeRules = @(
        # build-core.ps1 / build-msi.ps1 は、ビルドの進行を人が目で追うための
        # 実行ログを出す。Write-Output にするとパイプラインに流れて
        # 呼び出し元の戻り値を汚す。
        'PSAvoidUsingWriteHost',

        # 日本語コメントを含む UTF-8 ファイルに BOM を要求するルール。
        # pwsh 7 は BOM なし UTF-8 を既定で正しく読む。
        'PSUseBOMForUnicodeEncodedFile',

        # Join-Path a b のような呼び出しまで拾う。可読性の好みであって誤りではない。
        'PSAvoidUsingPositionalParameters'
    )
}
