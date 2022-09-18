# wgsocks

## TODO

- [ ] MTU auto calc (like other WG implementation does)
- [ ] IPv6 Support
- [ ] DNS Support

## LICENSE

MIT OR Apache-2.0

## internal

スレッドが複数生える

- [ ] メインスレッド
  - [x] SOCKS 受け入れスレッドを作成
  - [x] キューイングをひたすら待つ
  - [x] キューイングされたら…
      - [x] 新規コネクション？
        - [x] → TCP コネクションを貼る
      - [x] データ送信キューあり？
        - [x] → 送信する
      - [x] 接続切断キューあり？
        - [x] → smoltcp 側も切断する
      - [ ] エラー？
        - [ ] → ロギングしてエラー報告
    - [ ] コネクションをぐるっと回して
      - [x] コネクション貼り終わり？
        - [x] コネクションスレッドを作成
        - [x] → SOCKS のコネクション成功を返す
      - [x] データ受信キューあり？
        - [x] → 受信してクライアントに送信
      - [ ] 接続切断された？
        - [ ] クライアントの接続も切断
- [x] SOCKS受け入れスレッド
  - [x] クライアントからの接続を待つ
  - [x] 接続されたらさらにサブスレッドを生やす
    - [x] サブスレッドでは接続先の host/port を聞くところまでやってメインスレッドにキューイングする
- [x] コネクションスレッド
  - [x] クライアントからのデータ受信をひたすら待つ
  - [x] データを受信したら…
    - [x] メインスレッドに送信をキューイングする
  - [x] コネクションが切られたらその旨をキューイング
- [ ] WireGuard キープアライブ