fn main() {
    let img = std::fs::read("/tmp/m.img").unwrap();
    let sb = &img[0..4096];
    // rec_pool_start @56, obj_pool_start @32（小端 u64）
    let rec = u64::from_le_bytes(sb[56..64].try_into().unwrap());
    let pool = u64::from_le_bytes(sb[32..40].try_into().unwrap());
    println!("pool_start={pool} rec_start={rec}");
    let bmp = &img[(rec as usize)*4096..];
    // 位图块头 16B，位 122 → 字节 16+15，位 2
    let byte = bmp[16 + 122/8];
    println!("bit122={}（0=占用 1=空闲）byte={:#04x}", (byte >> (122%8)) & 1, byte);
    // 看 130..126 几位
    for b in 118..130 { print!("{} ", (bmp[16+b/8] >> (b%8)) & 1); }
    println!("（118..129 位，1=空闲）");
    // 对象头在 138？
    let h = &img[138*4096..138*4096+8];
    println!("blk138 头 4B = {:?}", &h[..4]);
}
